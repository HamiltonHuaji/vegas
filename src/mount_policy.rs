#[derive(Debug, Clone)]
pub struct MountPolicy {
    pub passthrough_dirs: Vec<&'static str>,
    pub extra_overlay_skip_prefixes: Vec<&'static str>,
    pub overlay_incompatible_fs_types: Vec<&'static str>,
}

impl Default for MountPolicy {
    fn default() -> Self {
        Self {
            passthrough_dirs: vec!["proc", "dev", "sys", "run", "var"],
            extra_overlay_skip_prefixes: vec!["/proc", "/dev", "/sys", "/run", "/var"],
            overlay_incompatible_fs_types: vec![
                "vfat", "msdos", "exfat", "ntfs", "ntfs3", "iso9660", "squashfs",
            ],
        }
    }
}
