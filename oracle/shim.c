/* oracle/shim.c -- FFI shim over the real libFLAC 1.4.3 encoder for the oracle
 * crate. One entry point encodes interleaved PCM in one of three modes (raw
 * frames, a complete FLAC stream, a complete Ogg FLAC stream) with an optional
 * list of metadata blocks, and returns the bytes in a malloc'd buffer the caller
 * releases with libflac_rs_oracle_free. */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "FLAC/stream_encoder.h"
#include "share/private.h" /* FLAC__stream_encoder_set_do_md5 */

enum { MODE_FRAMES = 0, MODE_STREAM = 1, MODE_OGG = 2 };

typedef struct {
    uint32_t channels;
    uint32_t bits_per_sample;
    uint32_t sample_rate;
    uint32_t blocksize;
    uint32_t compression_level;
    uint32_t do_md5;
    uint32_t mode;
    int32_t ogg_serial;
} oracle_config;

typedef struct {
    uint64_t offset;
    uint8_t number;
} oracle_index;

typedef struct {
    uint64_t offset;
    uint8_t number;
    uint8_t isrc[12];
    uint8_t non_audio;
    uint8_t pre_emphasis;
    uint32_t num_indices;
    const oracle_index *indices;
} oracle_track;

/* One metadata block; `type` is a FLAC__MetadataType and selects the fields that
 * apply. Strings libFLAC measures with strlen (mime type, description) are
 * NUL-terminated. */
typedef struct {
    uint32_t type;
    /* PADDING */
    uint32_t padding_length;
    /* APPLICATION (id + data), PICTURE (data) */
    uint8_t application_id[4];
    const uint8_t *data;
    uint32_t data_length;
    /* VORBIS_COMMENT */
    const uint8_t *vendor;
    uint32_t vendor_length;
    const uint8_t *const *comments;
    const uint32_t *comment_lengths;
    uint32_t num_comments;
    /* SEEKTABLE (template targets) */
    const uint64_t *seek_targets;
    uint32_t num_seek_targets;
    /* CUESHEET */
    const uint8_t *media_catalog_number; /* 128 bytes */
    uint64_t lead_in;
    uint32_t is_cd;
    const oracle_track *tracks;
    uint32_t num_tracks;
    /* PICTURE */
    uint32_t picture_type;
    const char *mime_type;
    const char *description;
    uint32_t width;
    uint32_t height;
    uint32_t depth;
    uint32_t colors;
} oracle_block;

/* A growable, seekable in-memory sink. In MODE_FRAMES it keeps only the audio
 * frames (write callback `samples > 0`), dropping the marker and metadata as CHD
 * does. */
typedef struct {
    uint8_t *buf;
    size_t cap;
    size_t pos;
    size_t len;
    int frames_only;
    int failed;
} sink;

static FLAC__StreamEncoderWriteStatus sink_write(const FLAC__StreamEncoder *encoder,
                                                 const FLAC__byte buffer[],
                                                 size_t bytes, uint32_t samples,
                                                 uint32_t current_frame,
                                                 void *client_data) {
    sink *s = (sink *)client_data;
    (void)encoder;
    (void)current_frame;
    if (s->frames_only && samples == 0) {
        return FLAC__STREAM_ENCODER_WRITE_STATUS_OK;
    }
    if (s->pos + bytes > s->cap) {
        size_t cap = s->cap ? s->cap : 65536;
        uint8_t *grown;
        while (cap < s->pos + bytes) {
            cap *= 2;
        }
        grown = (uint8_t *)realloc(s->buf, cap);
        if (!grown) {
            s->failed = 1;
            return FLAC__STREAM_ENCODER_WRITE_STATUS_FATAL_ERROR;
        }
        s->buf = grown;
        s->cap = cap;
    }
    memcpy(s->buf + s->pos, buffer, bytes);
    s->pos += bytes;
    if (s->pos > s->len) {
        s->len = s->pos;
    }
    return FLAC__STREAM_ENCODER_WRITE_STATUS_OK;
}

static FLAC__StreamEncoderSeekStatus sink_seek(const FLAC__StreamEncoder *encoder,
                                               FLAC__uint64 absolute_byte_offset,
                                               void *client_data) {
    sink *s = (sink *)client_data;
    (void)encoder;
    if (absolute_byte_offset > s->len) {
        return FLAC__STREAM_ENCODER_SEEK_STATUS_ERROR;
    }
    s->pos = (size_t)absolute_byte_offset;
    return FLAC__STREAM_ENCODER_SEEK_STATUS_OK;
}

static FLAC__StreamEncoderTellStatus sink_tell(const FLAC__StreamEncoder *encoder,
                                               FLAC__uint64 *absolute_byte_offset,
                                               void *client_data) {
    sink *s = (sink *)client_data;
    (void)encoder;
    *absolute_byte_offset = (FLAC__uint64)s->pos;
    return FLAC__STREAM_ENCODER_TELL_STATUS_OK;
}

/* The Ogg encoder reads its first page back at finish to rewrite STREAMINFO. */
static FLAC__StreamEncoderReadStatus sink_read(const FLAC__StreamEncoder *encoder,
                                               FLAC__byte buffer[], size_t *bytes,
                                               void *client_data) {
    sink *s = (sink *)client_data;
    size_t want = *bytes;
    (void)encoder;
    if (want > s->len - s->pos) {
        want = s->len - s->pos;
    }
    *bytes = want;
    if (want == 0) {
        return FLAC__STREAM_ENCODER_READ_STATUS_END_OF_STREAM;
    }
    memcpy(buffer, s->buf + s->pos, want);
    s->pos += want;
    return FLAC__STREAM_ENCODER_READ_STATUS_CONTINUE;
}

/* Fill `m` from `b`, allocating the arrays libFLAC needs (released by
 * free_block, also after a partial fill). Returns 0 on success. */
static int fill_block(FLAC__StreamMetadata *m, const oracle_block *b) {
    uint32_t i, j, length;
    memset(m, 0, sizeof(*m));
    m->type = (FLAC__MetadataType)b->type;
    m->is_last = false; /* the encoder sets it on the final block */
    switch (b->type) {
    case FLAC__METADATA_TYPE_PADDING:
        m->length = b->padding_length;
        return 0;
    case FLAC__METADATA_TYPE_APPLICATION:
        m->length = 4 + b->data_length;
        memcpy(m->data.application.id, b->application_id, 4);
        m->data.application.data = (FLAC__byte *)b->data;
        return 0;
    case FLAC__METADATA_TYPE_SEEKTABLE: {
        FLAC__StreamMetadata_SeekPoint *pts = NULL;
        if (b->num_seek_targets > 0) {
            pts = (FLAC__StreamMetadata_SeekPoint *)calloc(b->num_seek_targets,
                                                           sizeof(*pts));
            if (!pts) {
                return -1;
            }
        }
        for (i = 0; i < b->num_seek_targets; i++) {
            pts[i].sample_number = b->seek_targets[i];
        }
        m->length = b->num_seek_targets * FLAC__STREAM_METADATA_SEEKPOINT_LENGTH;
        m->data.seek_table.num_points = b->num_seek_targets;
        m->data.seek_table.points = pts;
        return 0;
    }
    case FLAC__METADATA_TYPE_VORBIS_COMMENT: {
        FLAC__StreamMetadata_VorbisComment_Entry *entries = NULL;
        length = 4 + b->vendor_length + 4;
        if (b->num_comments > 0) {
            entries = (FLAC__StreamMetadata_VorbisComment_Entry *)calloc(
                b->num_comments, sizeof(*entries));
            if (!entries) {
                return -1;
            }
        }
        for (i = 0; i < b->num_comments; i++) {
            entries[i].length = b->comment_lengths[i];
            entries[i].entry = (FLAC__byte *)b->comments[i];
            length += 4 + b->comment_lengths[i];
        }
        m->length = length;
        m->data.vorbis_comment.vendor_string.length = b->vendor_length;
        m->data.vorbis_comment.vendor_string.entry = (FLAC__byte *)b->vendor;
        m->data.vorbis_comment.num_comments = b->num_comments;
        m->data.vorbis_comment.comments = entries;
        return 0;
    }
    case FLAC__METADATA_TYPE_CUESHEET: {
        FLAC__StreamMetadata_CueSheet_Track *tracks = NULL;
        length = 396;
        if (b->num_tracks > 0) {
            tracks = (FLAC__StreamMetadata_CueSheet_Track *)calloc(b->num_tracks,
                                                                   sizeof(*tracks));
            if (!tracks) {
                return -1;
            }
        }
        m->data.cue_sheet.tracks = tracks;
        m->data.cue_sheet.num_tracks = b->num_tracks;
        for (i = 0; i < b->num_tracks; i++) {
            const oracle_track *t = &b->tracks[i];
            tracks[i].offset = t->offset;
            tracks[i].number = t->number;
            memcpy(tracks[i].isrc, t->isrc, 12);
            tracks[i].type = t->non_audio & 1;
            tracks[i].pre_emphasis = t->pre_emphasis & 1;
            tracks[i].num_indices = (FLAC__byte)t->num_indices;
            length += 36 + t->num_indices * 12;
            if (t->num_indices > 0) {
                tracks[i].indices = (FLAC__StreamMetadata_CueSheet_Index *)calloc(
                    t->num_indices, sizeof(*tracks[i].indices));
                if (!tracks[i].indices) {
                    return -1;
                }
            }
            for (j = 0; j < t->num_indices; j++) {
                tracks[i].indices[j].offset = t->indices[j].offset;
                tracks[i].indices[j].number = t->indices[j].number;
            }
        }
        m->length = length;
        memcpy(m->data.cue_sheet.media_catalog_number, b->media_catalog_number, 128);
        m->data.cue_sheet.lead_in = b->lead_in;
        m->data.cue_sheet.is_cd = b->is_cd != 0;
        return 0;
    }
    case FLAC__METADATA_TYPE_PICTURE:
        m->length = 32 + (uint32_t)strlen(b->mime_type) +
                    (uint32_t)strlen(b->description) + b->data_length;
        m->data.picture.type = (FLAC__StreamMetadata_Picture_Type)b->picture_type;
        m->data.picture.mime_type = (char *)b->mime_type;
        m->data.picture.description = (FLAC__byte *)b->description;
        m->data.picture.width = b->width;
        m->data.picture.height = b->height;
        m->data.picture.depth = b->depth;
        m->data.picture.colors = b->colors;
        m->data.picture.data = (FLAC__byte *)b->data;
        m->data.picture.data_length = b->data_length;
        return 0;
    default:
        return -1;
    }
}

static void free_block(FLAC__StreamMetadata *m) {
    uint32_t i;
    switch (m->type) {
    case FLAC__METADATA_TYPE_SEEKTABLE:
        free(m->data.seek_table.points);
        break;
    case FLAC__METADATA_TYPE_VORBIS_COMMENT:
        free(m->data.vorbis_comment.comments);
        break;
    case FLAC__METADATA_TYPE_CUESHEET:
        for (i = 0; m->data.cue_sheet.tracks && i < m->data.cue_sheet.num_tracks; i++) {
            free(m->data.cue_sheet.tracks[i].indices);
        }
        free(m->data.cue_sheet.tracks);
        break;
    default:
        break;
    }
}

/* Encode `nsamples` interleaved samples per `cfg`. With `num_blocks >= 0` the
 * blocks are passed to FLAC__stream_encoder_set_metadata (libFLAC still inserts
 * its own VORBIS_COMMENT when the list has none); with `num_blocks < 0` the
 * encoder's default metadata is used. On success returns 0 and hands over a
 * malloc'd buffer in *out / *out_len. On failure returns a negative code:
 * -1 allocation or bad block, -100-init_status, -200-state (process),
 * -300-state (finish). */
int libflac_rs_oracle_encode(const oracle_config *cfg, const int32_t *interleaved,
                             uint32_t nsamples, const oracle_block *blocks,
                             int32_t num_blocks, uint8_t **out, size_t *out_len) {
    sink s;
    FLAC__StreamEncoder *enc;
    FLAC__StreamEncoderInitStatus init;
    FLAC__StreamMetadata *metas = NULL;
    FLAC__StreamMetadata **meta_ptrs = NULL;
    int32_t i, filled = 0;
    int rc = 0;

    memset(&s, 0, sizeof(s));
    s.frames_only = cfg->mode == MODE_FRAMES;
    *out = NULL;
    *out_len = 0;

    enc = FLAC__stream_encoder_new();
    if (!enc) {
        return -1;
    }
    FLAC__stream_encoder_set_verify(enc, false);
    FLAC__stream_encoder_set_compression_level(enc, cfg->compression_level);
    FLAC__stream_encoder_set_channels(enc, cfg->channels);
    FLAC__stream_encoder_set_bits_per_sample(enc, cfg->bits_per_sample);
    FLAC__stream_encoder_set_sample_rate(enc, cfg->sample_rate);
    FLAC__stream_encoder_set_total_samples_estimate(enc, 0);
    FLAC__stream_encoder_set_streamable_subset(enc, false);
    FLAC__stream_encoder_set_blocksize(enc, cfg->blocksize);
    FLAC__stream_encoder_set_do_md5(enc, cfg->do_md5 != 0);
    if (cfg->mode == MODE_OGG) {
        FLAC__stream_encoder_set_ogg_serial_number(enc, cfg->ogg_serial);
    }

    if (num_blocks > 0) {
        metas = (FLAC__StreamMetadata *)calloc((size_t)num_blocks, sizeof(*metas));
        meta_ptrs = (FLAC__StreamMetadata **)calloc((size_t)num_blocks,
                                                    sizeof(*meta_ptrs));
        if (!metas || !meta_ptrs) {
            rc = -1;
            goto done;
        }
        for (i = 0; i < num_blocks; i++) {
            filled = i + 1;
            if (fill_block(&metas[i], &blocks[i]) != 0) {
                rc = -1;
                goto done;
            }
            meta_ptrs[i] = &metas[i];
        }
    }
    if (num_blocks >= 0) {
        FLAC__stream_encoder_set_metadata(enc, meta_ptrs, (uint32_t)num_blocks);
    }

    if (cfg->mode == MODE_OGG) {
        init = FLAC__stream_encoder_init_ogg_stream(enc, sink_read, sink_write,
                                                    sink_seek, sink_tell, NULL, &s);
    } else if (cfg->mode == MODE_STREAM) {
        init = FLAC__stream_encoder_init_stream(enc, sink_write, sink_seek, sink_tell,
                                                NULL, &s);
    } else {
        init = FLAC__stream_encoder_init_stream(enc, sink_write, NULL, NULL, NULL, &s);
    }
    if (init != FLAC__STREAM_ENCODER_INIT_STATUS_OK) {
        rc = -100 - (int)init;
    } else if (nsamples > 0 &&
               !FLAC__stream_encoder_process_interleaved(enc, interleaved, nsamples)) {
        rc = -200 - (int)FLAC__stream_encoder_get_state(enc);
    } else if (!FLAC__stream_encoder_finish(enc)) {
        rc = -300 - (int)FLAC__stream_encoder_get_state(enc);
    }

done:
    FLAC__stream_encoder_delete(enc);
    for (i = 0; i < filled; i++) {
        free_block(&metas[i]);
    }
    free(metas);
    free(meta_ptrs);
    if (rc == 0 && s.failed) {
        rc = -1;
    }
    if (rc != 0) {
        free(s.buf);
        return rc;
    }
    *out = s.buf;
    *out_len = s.len;
    return 0;
}

void libflac_rs_oracle_free(uint8_t *buf) { free(buf); }

/* FLAC__VENDOR_STRING of the compiled libFLAC. */
const char *libflac_rs_oracle_vendor_string(void) { return FLAC__VENDOR_STRING; }
