//! The NVIDIA render node used by the DMA-BUF capture path.
//!
//! KWin allocates the screencast DMA-BUFs on its render device; the bridge
//! only chooses which node to query modifiers on (EGL) and which device id
//! the NVENC encoder imports from. Both must be KWin's device, which on the
//! supported hosts is the only NVIDIA render node (docs/kde-findings.md
//! section 6). `KDE_RDP_GPU_DEVICE` selects a node explicitly.
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

/// A DRM render node and its device number (`st_rdev`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderNode {
    pub path: PathBuf,
    pub device: u64,
}

fn open(path: &Path) -> Option<u64> {
    let file = File::options().read(true).write(true).open(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat writes one stat structure for a valid descriptor.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat succeeded.
    let stat = unsafe { stat.assume_init() };
    (stat.st_mode & libc::S_IFMT == libc::S_IFCHR).then_some(stat.st_rdev)
}

fn nvidia(device: u64) -> bool {
    std::fs::read_to_string(format!(
        "/sys/dev/char/{}:{}/device/vendor",
        libc::major(device),
        libc::minor(device)
    ))
    .is_ok_and(|vendor| vendor.trim() == "0x10de")
}

/// The accessible NVIDIA render node, or `KDE_RDP_GPU_DEVICE`.
pub(crate) fn render_node() -> anyhow::Result<RenderNode> {
    if let Ok(path) = std::env::var("KDE_RDP_GPU_DEVICE") {
        anyhow::ensure!(
            path.starts_with("/dev/dri/renderD"),
            "KDE_RDP_GPU_DEVICE must be a DRM render node"
        );
        let path = PathBuf::from(path);
        let device =
            open(&path).ok_or_else(|| anyhow::anyhow!("{} is not accessible", path.display()))?;
        return Ok(RenderNode { path, device });
    }
    let mut paths: Vec<_> = std::fs::read_dir("/dev/dri")?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    for path in paths {
        if let Some(device) = open(&path).filter(|&device| nvidia(device)) {
            return Ok(RenderNode { path, device });
        }
    }
    anyhow::bail!("accessible NVIDIA render node unavailable")
}
