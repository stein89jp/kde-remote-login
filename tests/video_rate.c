/* Optional hardware regression: one live NVENC context across rate changes.
 * IDRs appear only at forced points: the first frame and each actual rate
 * reconfiguration (FFmpeg's NVENC rate change emits one). The long GOP adds
 * no periodic IDR within these five seconds. A final still phase proves the
 * quantiser floor: the average QP never goes below it, and once it is
 * reached a still frame costs next to nothing. */
#include <assert.h>
#include "../bridge/src/video.c"

int main(int argc, char **argv) {
    assert(argc == 2);
    char error[256] = {0};
    const int width = 640, height = 360, frames = 60;
    struct niri_video *video = niri_video_new(width, height, 0, 60, 12000000,
                                             error, sizeof(error));
    if (!video) { fprintf(stderr, "%s\n", error); return 1; }
    assert(video->codec->gop_size == NIRI_VIDEO_DEFAULT_GOP_SECONDS * 60);
    FILE *stream = fopen(argv[1], "wb");
    assert(stream);
    uint8_t *pixels = malloc((size_t)width * height * 4);
    assert(pixels);
    assert(video->codec->qmin == NIRI_VIDEO_DEFAULT_MIN_QP);
    const int rates[] = {12000000, 2000000, 8000000};
    const int still_frames = 120;
    size_t sizes[3] = {0};
    int keys = 0, lowest_qp = 99, last_qp = -1, last_size = 0;
    uint32_t random = 0x1badbeef;
    for (int phase = 0; phase < 4; ++phase) {
        /* The still phase repeats the last noise frame at the last rate. */
        if (phase < 3) assert(niri_video_set_bitrate(video, rates[phase]) == 0);
        for (int frame = 0; frame < (phase < 3 ? frames : still_frames); ++frame) {
            for (int i = 0; phase < 3 && i < width * height * 4; ++i) {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                pixels[i] = random;
            }
            const uint8_t *packet = NULL;
            int length = niri_video_encode(video, pixels, width * 4,
                                          phase == 0 && frame == 0,
                                          &packet, error, sizeof(error));
            if (length < 1) { fprintf(stderr, "%s\n", error); return 1; }
            assert(fwrite(packet, 1, length, stream) == (size_t)length);
            last_qp = niri_video_quantizer(video);
            last_size = length;
            if (last_qp < 0) { fprintf(stderr, "NVENC reported no frame quantiser\n"); return 1; }
            if (last_qp < lowest_qp) lowest_qp = last_qp;
            if (video->packet->flags & AV_PKT_FLAG_KEY) {
                if (frame != 0 || phase == 3) {
                    fprintf(stderr, "unexpected IDR at phase %d frame %d\n", phase, frame);
                    return 1;
                }
                ++keys;
            } else if (frame == 0 && phase > 0 && phase < 3) {
                fprintf(stderr, "rate change at phase %d did not start with an IDR\n", phase);
                return 1;
            }
            if (phase < 3) sizes[phase] += length;
        }
    }
    fclose(stream);
    free(pixels);
    niri_video_free(video);
    printf("NVENC phase bytes: %zu %zu %zu; IDRs: %d; lowest QP %d, settled still frame QP %d, "
           "%d bytes\n", sizes[0], sizes[1], sizes[2], keys, lowest_qp, last_qp, last_size);
    assert(keys == 3);
    /* The floor is the guarantee. How far a driver's rate control descends
       on a still is its own business, so the settled value is only printed. */
    assert(lowest_qp >= NIRI_VIDEO_DEFAULT_MIN_QP && last_qp <= NIRI_VIDEO_MAX_QP);
    assert(last_size < 1000);
    assert(sizes[1] < sizes[0] / 2);
    assert(sizes[2] > sizes[1] * 3 / 2);
    puts("PASS: live NVENC bitrate decreases and recovers; the quantiser floor holds");
    return 0;
}
