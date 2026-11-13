pub const SYSTEM_READ_DIRS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/usr/local/lib",
    "/usr/local/share",
    "/usr/share/locale",
    "/usr/share/zoneinfo",
    "/app/bin",
];

pub const PROC_READ_PATHS: &[&str] = &[
    "/proc/self",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/filesystems",
    "/proc/version",
    "/proc/version_signature",
    "/sys/fs/cgroup",
    "/sys/devices/system/cpu",
];

pub const READ_WRITE_DIRS: &[&str] = &["/tmp"];

pub const READ_WRITE_DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"];
