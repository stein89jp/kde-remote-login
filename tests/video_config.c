/* GOP and quantiser-floor configuration and descriptive native error text,
 * without NVIDIA hardware: the libx264 regression encoder shares the codec
 * setup used for NVENC. */
#include <assert.h>
#include "../bridge/src/video.c"

static void expect_error(const char *error, const char *prefix) {
    if (strncmp(error, prefix, strlen(prefix)) != 0) {
        fprintf(stderr, "expected error \"%s...\", got \"%s\"\n", prefix, error);
        exit(1);
    }
}

int main(void) {
    av_log_set_level(AV_LOG_QUIET);
    char error[256];
    const uint8_t *data = NULL, *aux = NULL;
    int main_size = 0, aux_size = 0;

    /* Stable fixture/benchmark entry points keep the default GOP (an hour:
       no periodic IDR in practice) and the default quantiser floor. */
    memset(error, 'x', sizeof(error));
    struct niri_video *v = niri_video_new_config(64, 64, 1, 60, 12000000, 1, 0, 0,
                                                 error, sizeof(error));
    assert(v && error[0] == '\0');
    assert(NIRI_VIDEO_DEFAULT_GOP_SECONDS == NIRI_VIDEO_MAX_GOP_SECONDS);
    assert(v->codec->gop_size == NIRI_VIDEO_DEFAULT_GOP_SECONDS * 60);
    assert(NIRI_VIDEO_DEFAULT_MIN_QP == 18 && v->codec->qmin == NIRI_VIDEO_DEFAULT_MIN_QP);
    assert(niri_video_quantizer(v) == -1 && niri_video_quantizer(NULL) == -1);
    niri_video_free(v);

    /* AVC444 doubles the codec rate; an even GOP keeps IDRs on main views. */
    v = niri_video_new_encoder(64, 64, 1, 30, 12000000, 1, 0, 1, 5, 24, error, sizeof(error));
    assert(v && v->codec->qmin == 24);
    assert(v->codec->gop_size == 5 * 30 * 2 && v->codec->gop_size % 2 == 0);
    int pixels_stride = 64 * 4;
    uint8_t *pixels = calloc(64 * 64, 4);
    assert(pixels);
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == 0);

    /* A failed color conversion reports its step instead of a bare -1. */
    struct SwsContext *valid = v->scale;
    v->scale = sws_getContext(64, 16, AV_PIX_FMT_BGR0, 64, 16, AV_PIX_FMT_YUV444P,
                              SWS_POINT, NULL, NULL, NULL);
    assert(v->scale);
    error[0] = '\0';
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 0, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == -1);
    expect_error(error, "convert AVC444 RGB to YUV444");
    sws_freeContext(v->scale);
    v->scale = valid;
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == 0);

    /* A context released by a failed GPU promotion is refused, not dereferenced. */
    avcodec_free_context(&v->codec);
    error[0] = '\0';
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == -1);
    expect_error(error, "H.264 encoder context released");
    assert(niri_video_set_bitrate(v, 4000000) == -1);
    assert(niri_video_set_rate(v, 4000000, 30) == -1);
    niri_video_free(v);

    v = niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, 1, 18, error, sizeof(error));
    assert(v && v->codec->gop_size == 60);
    avcodec_free_context(&v->codec);
    error[0] = '\0';
    assert(niri_video_encode(v, pixels, pixels_stride, 1, &data, error, sizeof(error)) == -1);
    expect_error(error, "H.264 encoder context released");
    niri_video_free(v);

    /* The GOP and quantiser knobs are bounded at the ABI as well as in the
       frontend. */
    assert(!niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, 0, 18, error, sizeof(error)));
    expect_error(error, "invalid H.264 dimensions, framerate, bitrate, GOP or minimum QP");
    assert(!niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0,
                                   NIRI_VIDEO_MAX_GOP_SECONDS + 1, 18, error, sizeof(error)));
    expect_error(error, "invalid H.264");
    for (int qp = 0; qp <= NIRI_VIDEO_MAX_QP + 1; qp += NIRI_VIDEO_MAX_QP + 1) {
        error[0] = '\0';
        assert(!niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, 30, qp,
                                       error, sizeof(error)));
        expect_error(error, "invalid H.264");
    }
    v = niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, NIRI_VIDEO_MAX_GOP_SECONDS,
                               NIRI_VIDEO_MAX_QP, error, sizeof(error));
    assert(v && v->codec->gop_size == NIRI_VIDEO_MAX_GOP_SECONDS * 60);
    assert(v->codec->qmin == NIRI_VIDEO_MAX_QP);
    niri_video_free(v);

    /* The floor binds and the frame's average quantiser is reported: noise
       at an ample bitrate would otherwise be coded far below 30. A still
       image then settles at the floor with nothing left to add. AVC444
       reports the higher of its two views. */
    for (int avc444 = 0; avc444 <= 1; ++avc444) {
        for (int floor = 1; floor <= 30; floor += 29) {
            v = niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, avc444, 30, floor,
                                       error, sizeof(error));
            assert(v);
            uint32_t random = 0x1badbeef;
            for (int i = 0; i < 64 * 64 * 4; ++i) {
                random ^= random << 13; random ^= random >> 17; random ^= random << 5;
                pixels[i] = random;
            }
            int lowest = 99, last = 0;
            for (int frame = 0; frame < 12; ++frame) {
                if (avc444) {
                    assert(niri_video_encode_pair(v, pixels, pixels_stride, frame == 0, &data,
                                                  &main_size, &aux, &aux_size, error,
                                                  sizeof(error)) == 0);
                    last = main_size + aux_size;
                } else {
                    last = niri_video_encode(v, pixels, pixels_stride, frame == 0, &data,
                                             error, sizeof(error));
                    assert(last > 0);
                }
                int qp = niri_video_quantizer(v);
                assert(qp >= 0 && qp <= NIRI_VIDEO_MAX_QP);
                if (qp < lowest) lowest = qp;
            }
            if (floor == 30) {
                assert(lowest == 30 && niri_video_quantizer(v) == 30);
                assert(last < 64 * 64 / 8);
            } else {
                assert(lowest < 30);
            }
            niri_video_free(v);
        }
    }
    memset(pixels, 0, 64 * 64 * 4);

    double conversion = -1, encode = -1;
    niri_video_timings(NULL, &conversion, &encode);
    assert(conversion == 0 && encode == 0);
    free(pixels);
    puts("PASS: GOP and quantiser-floor bounds/defaults and descriptive native encoder errors");
    return 0;
}
