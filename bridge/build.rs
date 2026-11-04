fn main() {
    println!("cargo:rerun-if-changed=src/audio.c");
    println!("cargo:rerun-if-changed=src/video.c");
    println!("cargo:rerun-if-changed=src/video_gpu.c");
    let mut build = cc::Build::new();
    build.file("src/audio.c");
    build.file("src/video.c").flag_if_supported("-std=c11");
    // EGL/OpenGL import and CUDA driver interop is an optional capture path;
    // the encoder still provides its normal SHM fallback.
    for name in ["egl", "gl"] {
        let library = pkg_config::Config::new()
            .probe(name)
            .unwrap_or_else(|e| panic!("GPU development library {name} required: {e}"));
        for path in library.include_paths {
            build.include(path);
        }
    }
    build.define("NIRI_VIDEO_GPU", None);
    for name in ["libavcodec", "libavutil", "libswscale", "libswresample"] {
        let library = pkg_config::Config::new()
            .probe(name)
            .unwrap_or_else(|e| panic!("FFmpeg development library {name} required: {e}"));
        for path in library.include_paths {
            build.include(path);
        }
    }
    build.compile("kde_rdp_video");

    // KWin screencast consumer: PipeWire over the broker's fd, plus the EGL
    // DMA-BUF modifier query for the render node the encoder imports from.
    println!("cargo:rerun-if-changed=src/screencast.c");
    let mut screencast = cc::Build::new();
    screencast
        .file("src/screencast.c")
        .flag_if_supported("-std=gnu11");
    for name in ["libpipewire-0.3", "egl"] {
        let library = pkg_config::Config::new()
            .probe(name)
            .unwrap_or_else(|e| panic!("screencast development library {name} required: {e}"));
        for path in library.include_paths {
            screencast.include(path);
        }
        for (key, value) in library.defines {
            screencast.define(&key, value.as_deref());
        }
    }
    screencast.compile("kde_rdp_screencast");
}
