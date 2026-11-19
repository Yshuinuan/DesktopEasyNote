//! 应用数据目录与路径（唯一路径来源）。
//!
//! 根目录：`%LOCALAPPDATA%\DesktopEasyNote`（R16 / 契约 v1.6 起；旧版本为 `%LOCALAPPDATA%\桌面旁白记录器`）
//! - `data/`   记录 `YYYY-MM-DD.md`
//! - `output/` 旁白 `YYYY-MM-DD.md`
//! - `logs/`   日志 `app-YYYY-MM-DD.log`
//! - `config.json` 配置（apikey/baseURL/model）
//!
//! `LOCALAPPDATA` 缺失时回退 `APPDATA`，再缺失回退当前目录，并记录 warning。
//! 业务代码不得硬编码任何路径，一律通过 `paths::get()`。
//!
//! # R16 一次性迁移（只复制、不删除）
//! - 旧根：`%LOCALAPPDATA%\桌面旁白记录器`（按同样的回退规则推导）。
//! - 触发：新根**既无 `config.json` 也无 `data/`**，且旧根存在。
//! - 复制 `config.json`、`data/**`、`output/**`（**不迁移 `logs/`**），逐文件校验 size + SHA-256。
//! - 成功：本次使用新根；旧根 `config.json`/`data/`/`output/`/`logs/` 全部原样保留（不删除）。
//! - 失败：回滚新根本次写入的不完整副本、写 ERROR，本次启动继续使用旧根，下次启动重试。
//! - 新根已有数据：不迁移、不覆盖，本次使用新根，旧根保留。
//!
//! 迁移在 `init()` 内完成（早于日志初始化），结果缓存在 `MIGRATION_REPORT`，
//! 由 `lib.rs` 在日志就绪后写成 INFO/ERROR；业务代码仍只用 `get()`，无需感知迁移。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 新数据根目录名（R16：英文路径）。
pub const ROOT_DIR_NAME: &str = "DesktopEasyNote";

/// 旧数据根目录名（R16 迁移来源；只读，迁移后也不删除）。
pub const LEGACY_ROOT_DIR_NAME: &str = "桌面旁白记录器";

/// 迁移范围：只复制旧根下的这些子目录（`logs/` 明确不迁移）。
const MIGRATION_SUBDIRS: [&str; 2] = ["data", "output"];

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub config_file: PathBuf,
}

impl AppPaths {
    /// 由根目录推导出全部路径（供测试与 `resolve` 共用）。
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            data_dir: root.join("data"),
            output_dir: root.join("output"),
            logs_dir: root.join("logs"),
            config_file: root.join("config.json"),
            root,
        }
    }

    /// 由同一个基目录推导新旧两个根（R16：旧根用同样回退规则得到同一个基目录）。
    pub fn roots_from_base(base: &Path) -> (Self, Self) {
        (
            Self::from_root(base.join(ROOT_DIR_NAME)),
            Self::from_root(base.join(LEGACY_ROOT_DIR_NAME)),
        )
    }

    /// 启动时确保根目录与三个子目录存在；失败只返回错误描述，不崩溃。
    pub fn ensure_dirs(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for dir in [&self.root, &self.data_dir, &self.output_dir, &self.logs_dir] {
            if let Err(error) = fs::create_dir_all(dir) {
                errors.push(format!("创建目录失败 {}: {error}", dir.display()));
            }
        }
        errors
    }

    /// 记录文件：`data/YYYY-MM-DD.md`（文件名用「这天的日期」）。
    pub fn data_file(&self, day_key: &str) -> PathBuf {
        self.data_dir.join(format!("{day_key}.md"))
    }

    /// 旁白输出：`output/YYYY-MM-DD.md`（文件名同记录日期）。
    pub fn output_file(&self, day_key: &str) -> PathBuf {
        self.output_dir.join(format!("{day_key}.md"))
    }
}

// ---------------------------------------------------------------------------
// 基目录回退规则（LOCALAPPDATA → APPDATA → 当前目录）
// ---------------------------------------------------------------------------

/// 纯函数版的基目录解析：方便在测试中注入环境值，**不读取真实环境变量**。
fn base_dir_from(
    local_app_data: Option<PathBuf>,
    app_data: Option<PathBuf>,
    current_dir: &Path,
) -> (PathBuf, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();

    let non_empty = |value: Option<PathBuf>| value.filter(|path| !path.as_os_str().is_empty());

    if let Some(value) = non_empty(local_app_data) {
        return (value, warnings);
    }
    warnings.push("LOCALAPPDATA 缺失，回退到 APPDATA".to_string());

    if let Some(value) = non_empty(app_data) {
        return (value, warnings);
    }
    warnings.push("APPDATA 也缺失，回退到当前工作目录（数据可能不稳定）".to_string());

    (current_dir.to_path_buf(), warnings)
}

/// 读取真实环境变量解析基目录（仅生产启动路径使用）。
fn resolve_base() -> (PathBuf, Vec<String>) {
    let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    base_dir_from(
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
        env::var_os("APPDATA").map(PathBuf::from),
        &current_dir,
    )
}

// ---------------------------------------------------------------------------
// R16 迁移报告
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationStatus {
    /// 新根已有数据：不迁移、不覆盖，使用新根（旧根保留）。
    SkippedNewRootHasData,
    /// 旧根不存在：直接使用新根。
    NoOldRoot,
    /// 迁移复制 + 校验成功：使用新根，旧根原样保留。
    Migrated,
    /// 迁移失败：本次启动继续使用旧根，下次启动重试。
    FailedKeepOldRoot,
}

impl MigrationStatus {
    /// 本次启动是否使用新根。
    pub fn uses_new_root(self) -> bool {
        !matches!(self, MigrationStatus::FailedKeepOldRoot)
    }

    /// 日志里的短标签（排错用）。
    pub fn label(self) -> &'static str {
        match self {
            MigrationStatus::SkippedNewRootHasData => "skip-new-root-has-data",
            MigrationStatus::NoOldRoot => "no-old-root",
            MigrationStatus::Migrated => "migrated",
            MigrationStatus::FailedKeepOldRoot => "failed-keep-old-root",
        }
    }
}

/// 一次启动的迁移结果；`paths::migration_report()` 供 `lib.rs` 在日志初始化后输出。
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub status: MigrationStatus,
    pub new_root: PathBuf,
    pub old_root: PathBuf,
    pub old_root_existed: bool,
    pub file_count: usize,
    pub total_bytes: u64,
    /// 是否逐文件 size + SHA-256 全部校验一致。
    pub verified: bool,
    pub errors: Vec<String>,
}

impl MigrationReport {
    fn new(status: MigrationStatus, new_root: &AppPaths, old_root: &AppPaths) -> Self {
        Self {
            status,
            new_root: new_root.root.clone(),
            old_root: old_root.root.clone(),
            old_root_existed: old_root.root.is_dir(),
            file_count: 0,
            total_bytes: 0,
            verified: false,
            errors: Vec::new(),
        }
    }

    /// 迁移失败时写 ERROR 的日志行（其余状态为空）。
    pub fn error_lines(&self) -> Vec<String> {
        if self.status != MigrationStatus::FailedKeepOldRoot {
            return Vec::new();
        }
        vec![format!(
            "数据目录迁移失败（R16，{}）：{}；本次启动继续使用旧根 {}（旧数据未删除），下次启动重试；新根 {} 的不完整副本已回滚清理",
            self.status.label(),
            self.errors.join("；"),
            self.old_root.display(),
            self.new_root.display()
        )]
    }

    /// 迁移结论写 INFO 的日志行（成功一条；跳过/无旧根各一条；失败只写 ERROR）。
    pub fn info_lines(&self) -> Vec<String> {
        match self.status {
            MigrationStatus::Migrated => vec![format!(
                "数据目录迁移完成（R16）：复制 config.json/data/output 共 {} 个文件、{} 字节，逐文件 size+SHA-256 校验全部一致；本次使用新根 {}；不迁移 logs/，旧目录 {} 的 config.json/data/output/logs 全部原样保留（未删除），确认新版正常后可自行清理",
                self.file_count,
                self.total_bytes,
                self.new_root.display(),
                self.old_root.display()
            )],
            MigrationStatus::SkippedNewRootHasData => {
                if self.old_root_existed {
                    vec![format!(
                        "新数据目录已有数据，跳过迁移（R16）：本次使用新根 {}，不迁移、不覆盖；旧目录 {} 原样保留（未删除），确认新版正常后可自行清理",
                        self.new_root.display(),
                        self.old_root.display()
                    )]
                } else {
                    vec![format!(
                        "新数据目录 {} 已有数据，直接使用（R16）；未发现旧目录 {}，无需迁移",
                        self.new_root.display(),
                        self.old_root.display()
                    )]
                }
            }
            MigrationStatus::NoOldRoot => vec![format!(
                "未发现旧数据目录 {}（R16 迁移来源），直接使用新根 {}",
                self.old_root.display(),
                self.new_root.display()
            )],
            MigrationStatus::FailedKeepOldRoot => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// R16 一次性复制迁移
// ---------------------------------------------------------------------------

/// 仅测试用的故障注入：模拟「复制后目标被篡改」，验证 size + SHA-256 校验与失败回退。
#[derive(Clone, Copy, Default)]
struct FaultInjection {
    #[cfg(test)]
    tamper: TamperMode,
}

#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum TamperMode {
    #[default]
    None,
    /// 目标文件末尾追加一个字节（size 不一致）。
    AppendByte,
    /// 目标文件首字节取反（size 一致但 SHA-256 不一致）。
    FlipByte,
}

/// 迁移失败时回滚「本次写入新根」的内容；只删本次复制出的文件与随之变空的目录。
fn rollback_partial(new_root: &AppPaths, planned: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (_, relative) in planned {
        let target = new_root.root.join(relative);
        if target.is_file() {
            if let Err(error) = fs::remove_file(&target) {
                errors.push(format!("删除不完整副本失败 {}: {error}", target.display()));
            }
        }
    }
    // 只删除变空的目录；新根里预先存在的文件（如日志）会让目录保留，不会被误删。
    remove_empty_dirs(&new_root.root);
    errors
}

/// 自底向上删除空目录；非空目录删不掉即忽略，绝不删除文件。
fn remove_empty_dirs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_empty_dirs(&path);
        }
    }
    let _ = fs::remove_dir(dir);
}

/// 收集迁移清单：旧根 `config.json` + `data/**` + `output/**`（**不含 `logs/`**，也不含旧根其他文件）。
///
/// 返回 `(源文件绝对路径, 相对新根的路径)`。
fn collect_migration_files(old_root: &AppPaths) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    if old_root.config_file.is_file() {
        files.push((old_root.config_file.clone(), PathBuf::from("config.json")));
    }
    for dir_name in MIGRATION_SUBDIRS {
        let source_dir = old_root.root.join(dir_name);
        if source_dir.is_dir() {
            collect_dir_files(&source_dir, PathBuf::from(dir_name), &mut files)?;
        }
    }
    Ok(files)
}

fn collect_dir_files(
    source_dir: &Path,
    relative_dir: PathBuf,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let entries = fs::read_dir(source_dir)
        .map_err(|error| format!("读取旧目录失败 {}: {error}", source_dir.display()))?;
    let mut entries = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取旧目录失败 {}: {error}", source_dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = relative_dir.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取文件类型失败 {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_dir_files(&path, relative, files)?;
        } else if file_type.is_file() {
            files.push((path, relative));
        }
        // 符号链接等其它类型不迁移，避免越出旧根。
    }
    Ok(())
}

/// 复制单个文件并校验 size + SHA-256（源/目标一致）。目标已存在时拒绝覆盖。
fn copy_and_verify_file(
    source: &Path,
    target: &Path,
    _fault: FaultInjection,
) -> Result<u64, String> {
    if target.exists() {
        return Err(format!("目标已存在，拒绝覆盖: {}", target.display()));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建目标目录失败 {}: {error}", parent.display()))?;
    }

    let source_size = fs::metadata(source)
        .map_err(|error| format!("读取源文件信息失败 {}: {error}", source.display()))?
        .len();
    fs::copy(source, target).map_err(|error| {
        format!(
            "复制失败 {} -> {}: {error}",
            source.display(),
            target.display()
        )
    })?;

    #[cfg(test)]
    inject_tamper(target, _fault.tamper);

    let target_size = fs::metadata(target)
        .map_err(|error| format!("读取目标文件信息失败 {}: {error}", target.display()))?
        .len();
    if target_size != source_size {
        return Err(format!(
            "size 不一致（源 {source_size} B，目标 {target_size} B）"
        ));
    }

    let source_hash = sha256_file(source)?;
    let target_hash = sha256_file(target)?;
    if source_hash != target_hash {
        return Err(format!(
            "SHA-256 不一致（源 {source_hash}，目标 {target_hash}）"
        ));
    }
    Ok(source_size)
}

#[cfg(test)]
fn inject_tamper(target: &Path, mode: TamperMode) {
    match mode {
        TamperMode::None => {}
        TamperMode::AppendByte => {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().append(true).open(target) {
                let _ = file.write_all(b"X");
            }
        }
        TamperMode::FlipByte => {
            if let Ok(mut bytes) = fs::read(target) {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 0x01;
                } else {
                    bytes.push(b'X');
                }
                let _ = fs::write(target, bytes);
            }
        }
    }
}

/// 执行 R16 一次性迁移，返回本次启动应使用的路径与报告。
fn migrate(new_root: &AppPaths, old_root: &AppPaths, fault: FaultInjection) -> MigrationReport {
    // 1) 新根已有数据（config.json 或 data/）：不迁移、不覆盖，直接用新根。
    let new_root_has_data = new_root.config_file.is_file() || new_root.data_dir.is_dir();
    if new_root_has_data {
        return MigrationReport::new(MigrationStatus::SkippedNewRootHasData, new_root, old_root);
    }

    // 2) 旧根不存在（或不是目录）：正常以新根运行。
    if !old_root.root.is_dir() {
        return MigrationReport::new(MigrationStatus::NoOldRoot, new_root, old_root);
    }

    // 3) 触发迁移：只复制、不移动；逐个文件复制后校验。
    let mut report = MigrationReport::new(MigrationStatus::FailedKeepOldRoot, new_root, old_root);
    let planned = match collect_migration_files(old_root) {
        Ok(planned) => planned,
        Err(error) => {
            report.errors.push(error);
            return report;
        }
    };

    // 预检：新根里已有同名文件时拒绝覆盖（保护新根既有数据）。
    for (_, relative) in &planned {
        let target = new_root.root.join(relative);
        if target.exists() {
            report.errors.push(format!(
                "新根目标已存在，拒绝覆盖: {}（未写入任何文件）",
                target.display()
            ));
            return report;
        }
    }

    let prepare = || -> Result<(), String> {
        fs::create_dir_all(&new_root.root)
            .map_err(|error| format!("创建新根失败 {}: {error}", new_root.root.display()))?;
        for dir_name in MIGRATION_SUBDIRS {
            let dir = new_root.root.join(dir_name);
            fs::create_dir_all(&dir)
                .map_err(|error| format!("创建目标目录失败 {}: {error}", dir.display()))?;
        }
        Ok(())
    };
    if let Err(error) = prepare() {
        report.errors.push(error);
        report
            .errors
            .extend(rollback_partial(new_root, &planned));
        return report;
    }

    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    for (source, relative) in &planned {
        let target = new_root.root.join(relative);
        match copy_and_verify_file(source, &target, fault) {
            Ok(size) => {
                file_count += 1;
                total_bytes += size;
            }
            Err(error) => {
                report
                    .errors
                    .push(format!("文件 {}: {error}", relative.display()));
                // 任一文件失败：回滚本次不完整副本，本次启动用旧根。
                let rollback_errors = rollback_partial(new_root, &planned);
                report.errors.extend(
                    rollback_errors
                        .into_iter()
                        .map(|error| format!("回滚清理失败: {error}")),
                );
                return report;
            }
        }
    }

    report.status = MigrationStatus::Migrated;
    report.file_count = file_count;
    report.total_bytes = total_bytes;
    report.verified = true;
    report
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

struct InitOutcome {
    /// 本次启动实际使用的路径（迁移失败时是旧根）。
    paths: AppPaths,
    warnings: Vec<String>,
    report: MigrationReport,
}

/// 纯函数版初始化流程（测试注入临时基目录，**不读取真实环境变量、不碰真实 LOCALAPPDATA**）。
fn resolve_with(base: PathBuf, mut warnings: Vec<String>, fault: FaultInjection) -> InitOutcome {
    let (new_root, old_root) = AppPaths::roots_from_base(&base);
    let report = migrate(&new_root, &old_root, fault);
    let paths = if report.status.uses_new_root() {
        new_root
    } else {
        old_root
    };
    warnings.extend(paths.ensure_dirs());
    InitOutcome {
        paths,
        warnings,
        report,
    }
}

static PATHS: OnceLock<AppPaths> = OnceLock::new();
static PATHS_WARNINGS: OnceLock<Vec<String>> = OnceLock::new();
static MIGRATION_REPORT: OnceLock<MigrationReport> = OnceLock::new();

/// 初始化（幂等）：解析路径、执行 R16 一次性迁移、创建目录，并缓存 warning 与迁移报告。
///
/// 迁移在日志初始化之前完成，因此这里**不写日志**；结果通过 [`migration_report`] 取回，
/// 由 `lib.rs` 在 `logging::init` 之后输出 INFO/ERROR。
pub fn init() -> &'static AppPaths {
    PATHS.get_or_init(|| {
        let (base, base_warnings) = resolve_base();
        let outcome = resolve_with(base, base_warnings, FaultInjection::default());
        let _ = PATHS_WARNINGS.set(outcome.warnings);
        let _ = MIGRATION_REPORT.set(outcome.report);
        outcome.paths
    })
}

/// 全局唯一路径入口；未显式 init 时也会惰性初始化。
pub fn get() -> &'static AppPaths {
    init()
}

/// 取出初始化期间累积的 warning（供启动时写日志）。
pub fn init_warnings() -> Vec<String> {
    let _ = init();
    PATHS_WARNINGS.get().cloned().unwrap_or_default()
}

/// 取出本次启动的 R16 迁移报告（供 `lib.rs` 在日志初始化后输出）；未初始化时先触发初始化。
pub fn migration_report() -> Option<MigrationReport> {
    let _ = init();
    MIGRATION_REPORT.get().cloned()
}

// ---------------------------------------------------------------------------
// SHA-256（迁移校验用；Cargo.toml 冻结不新增依赖，故内置实现）
// ---------------------------------------------------------------------------

mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut state = H0;
        let bit_len = (data.len() as u64).wrapping_mul(8);

        let mut chunks = data.chunks_exact(64);
        for chunk in &mut chunks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            compress(&mut state, &block);
        }

        // 尾部补齐：0x80 + 0 填充 + 64 位大端消息长度；余数 ≥ 56 时占两个块。
        let remainder = chunks.remainder();
        let mut tail = [0u8; 128];
        tail[..remainder.len()].copy_from_slice(remainder);
        tail[remainder.len()] = 0x80;
        let tail_len = if remainder.len() < 56 { 64 } else { 128 };
        tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_be_bytes());
        for block in tail[..tail_len].chunks_exact(64) {
            let mut block_bytes = [0u8; 64];
            block_bytes.copy_from_slice(block);
            compress(&mut state, &block_bytes);
        }

        let mut output = [0u8; 32];
        for (index, word) in state.iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    pub fn hex(digest: &[u8; 32]) -> String {
        let mut text = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(text, "{byte:02x}");
        }
        text
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut schedule = [0u32; 64];
        for index in 0..16 {
            schedule[index] = u32::from_be_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
}

/// 读取文件计算 SHA-256（十六进制小写）。
fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("读取文件失败 {}: {error}", path.display()))?;
    Ok(sha256::hex(&sha256::digest(&bytes)))
}

// ---------------------------------------------------------------------------
// 单测：全部用临时目录模拟新旧根，绝不读取/写入真实 `%LOCALAPPDATA%`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 测试用临时目录；`Drop` 时递归删除。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "desktop-narrator-paths-{tag}-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建测试父目录");
        }
        fs::write(path, content).expect("写测试文件");
    }

    /// 目录树快照：相对路径 → (size, SHA-256)，用于断言「旧目录未变」与「新目录一致」。
    fn snapshot(root: &Path) -> BTreeMap<String, (u64, String)> {
        fn walk(base: &Path, dir: &Path, map: &mut BTreeMap<String, (u64, String)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, map);
                } else if path.is_file() {
                    let relative = path
                        .strip_prefix(base)
                        .expect("相对路径")
                        .to_string_lossy()
                        .replace('\\', "/");
                    let size = fs::metadata(&path).expect("读元数据").len();
                    let hash = sha256_file(&path).expect("算哈希");
                    map.insert(relative, (size, hash));
                }
            }
        }
        let mut map = BTreeMap::new();
        walk(root, root, &mut map);
        map
    }

    /// 在旧根写下覆盖 config/data(含子目录)/output/logs 以及「不该迁移的其它文件」的样例数据。
    fn seed_old_root(old_root: &AppPaths) {
        write_file(
            &old_root.config_file,
            br#"{"api_key":"sk-test","base_url":"https://api.deepseek.com","model":"deepseek-chat"}"#,
        );
        write_file(
            &old_root.data_dir.join("2026-11-16.md"),
            "08:47 · 醒了\n10:12 · 写代码 · 做成了\n".as_bytes(),
        );
        write_file(
            &old_root.data_dir.join("2026-11-17.md"),
            "07:30 · 早起\n".as_bytes(),
        );
        write_file(
            &old_root
                .data_dir
                .join("archive")
                .join("2026-11-15.md"),
            "23:59 · 旧文件\n".as_bytes(),
        );
        write_file(
            &old_root.output_dir.join("2026-11-16.md"),
            "他抬了下眼皮。\n".as_bytes(),
        );
        write_file(
            &old_root.logs_dir.join("app-2026-11-16.log"),
            "[2026-11-16 08:00:00.000] INFO  应用启动\r\n".as_bytes(),
        );
        write_file(&old_root.root.join("desktop-narrator.exe"), b"fake-exe");
        write_file(&old_root.root.join("uninstall.exe"), b"fake-uninstall");
    }

    fn seed_new_root_with_data(new_root: &AppPaths) {
        write_file(&new_root.config_file, b"{\"api_key\":\"sk-new\"}");
        write_file(&new_root.data_dir.join("2026-11-16.md"), "09:00 · 新根\n".as_bytes());
    }

    // ---- SHA-256 自检（NIST 已知向量） --------------------------------------

    #[test]
    fn sha256_matches_nist_known_vectors() {
        assert_eq!(
            sha256::hex(&sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex(&sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex(&sha256::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1,000,000 个 'a'：覆盖多块与末尾整块边界。
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256::hex(&sha256::digest(&million_a)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // ---- 回退规则 ----------------------------------------------------------

    #[test]
    fn fallback_rules_match_localappdata_appdata_cwd_order() {
        let cwd = PathBuf::from(r"C:\fallback\cwd");
        let local = PathBuf::from(r"C:\fallback\local");
        let roaming = PathBuf::from(r"C:\fallback\roaming");

        let (base, warnings) = base_dir_from(Some(local.clone()), Some(roaming.clone()), &cwd);
        assert_eq!(base, local);
        assert!(warnings.is_empty());

        let (base, warnings) = base_dir_from(None, Some(roaming.clone()), &cwd);
        assert_eq!(base, roaming);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("LOCALAPPDATA 缺失"));
        assert!(warnings[0].contains("APPDATA"));

        let (base, warnings) = base_dir_from(None, None, &cwd);
        assert_eq!(base, cwd);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[1].contains("APPDATA 也缺失"));
        assert!(warnings[1].contains("当前工作目录"));

        // 空字符串按缺失处理。
        let (base, warnings) = base_dir_from(Some(PathBuf::new()), None, &cwd);
        assert_eq!(base, cwd);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn roots_use_english_dir_and_legacy_chinese_dir() {
        assert_eq!(ROOT_DIR_NAME, "DesktopEasyNote");
        assert_eq!(LEGACY_ROOT_DIR_NAME, "桌面旁白记录器");

        let (new_root, old_root) = AppPaths::roots_from_base(Path::new(r"C:\base"));
        assert_eq!(new_root.root, PathBuf::from(r"C:\base\DesktopEasyNote"));
        assert_eq!(
            old_root.root,
            PathBuf::from(r"C:\base\桌面旁白记录器")
        );
        assert_eq!(
            new_root.config_file,
            PathBuf::from(r"C:\base\DesktopEasyNote\config.json")
        );
        assert_eq!(new_root.data_file("2026-11-19"), new_root.data_dir.join("2026-11-19.md"));
        assert_eq!(
            new_root.output_file("2026-11-19"),
            new_root.output_dir.join("2026-11-19.md")
        );
    }

    // ---- 正常迁移 ----------------------------------------------------------

    #[test]
    fn migrate_copies_config_data_output_verifies_and_skips_logs() {
        let temp = TempDir::new("migrate-ok");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);

        let old_before = snapshot(&old_root.root);
        let outcome = resolve_with(base.clone(), Vec::new(), FaultInjection::default());

        // 迁移成功，本次使用新根。
        assert_eq!(outcome.report.status, MigrationStatus::Migrated);
        assert!(outcome.report.verified);
        assert!(outcome.report.errors.is_empty());
        assert_eq!(outcome.paths.root, new_root.root);
        assert!(outcome.warnings.is_empty());

        // 只复制 config.json + data/** + output/**（2 个 data 文件 + 1 个 data 子目录文件 + 1 个 output + config = 5）。
        assert_eq!(outcome.report.file_count, 5);
        let expected_bytes: u64 = ["config.json", "data/2026-11-16.md", "data/2026-11-17.md", "data/archive/2026-11-15.md", "output/2026-11-16.md"]
            .iter()
            .map(|relative| old_before.get(*relative).expect("旧文件存在").0)
            .sum();
        assert_eq!(outcome.report.total_bytes, expected_bytes);

        // 新根文件与旧根哈希一致（逐文件 size + SHA-256）。
        for relative in [
            "config.json",
            "data/2026-11-16.md",
            "data/2026-11-17.md",
            "data/archive/2026-11-15.md",
            "output/2026-11-16.md",
        ] {
            let new_path = new_root.root.join(relative);
            let old_path = old_root.root.join(relative);
            assert!(new_path.is_file(), "新根缺文件 {relative}");
            assert_eq!(sha256_file(&new_path).unwrap(), sha256_file(&old_path).unwrap());
            assert_eq!(
                fs::metadata(&new_path).unwrap().len(),
                fs::metadata(&old_path).unwrap().len()
            );
        }

        // logs 只留在旧目录：新根 logs 目录存在（启动确保）但为空。
        let new_logs: Vec<_> = fs::read_dir(&new_root.logs_dir)
            .expect("新根 logs 目录应存在")
            .collect();
        assert!(new_logs.is_empty(), "logs 不应迁移到新根");
        assert!(old_root.logs_dir.join("app-2026-11-16.log").is_file());

        // 旧根其它文件（exe）不迁移。
        assert!(!new_root.root.join("desktop-narrator.exe").exists());
        assert!(!new_root.root.join("uninstall.exe").exists());

        // 旧目录整体原样保留（含 logs 与 exe），哈希不变。
        assert_eq!(snapshot(&old_root.root), old_before);
    }

    #[test]
    fn migrate_success_writes_single_info_line_with_counts() {
        let temp = TempDir::new("report-ok");
        let base = temp.path().to_path_buf();
        let (_, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        let info = outcome.report.info_lines();
        assert_eq!(info.len(), 1, "迁移成功只写一条 INFO");
        assert!(info[0].contains("5 个文件"));
        assert!(info[0].contains(&format!("{} 字节", outcome.report.total_bytes)));
        assert!(info[0].contains("SHA-256"));
        assert!(info[0].contains("不迁移 logs/"));
        assert!(info[0].contains("原样保留"));
        assert!(outcome.report.error_lines().is_empty());
    }

    #[test]
    fn migrate_never_deletes_or_rewrites_old_tree() {
        let temp = TempDir::new("keep-old");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);

        // 先记录旧目录每个文件的字节内容。
        let old_before = snapshot(&old_root.root);
        let config_bytes = fs::read(&old_root.config_file).unwrap();

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::Migrated);

        // 旧目录文件全部仍在，哈希与内容不变（要求：只复制、不删除）。
        let old_after = snapshot(&old_root.root);
        assert_eq!(old_after, old_before);
        assert_eq!(fs::read(&old_root.config_file).unwrap(), config_bytes);

        // 新根是独立副本，改动新根不会影响旧根。
        write_file(
            &new_root.data_dir.join("2026-11-16.md"),
            "修改新根副本\n".as_bytes(),
        );
        assert_eq!(snapshot(&old_root.root), old_before);
    }

    // ---- 失败回退 ----------------------------------------------------------

    /// 校验失败（目标被篡改、size 一致但 SHA-256 不一致）→ 本次用旧根；下次启动重试成功。
    #[test]
    fn migrate_hash_mismatch_falls_back_to_old_root_then_retries() {
        let temp = TempDir::new("hash-fail");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);
        let old_before = snapshot(&old_root.root);

        let outcome = resolve_with(
            base.clone(),
            Vec::new(),
            FaultInjection {
                tamper: TamperMode::FlipByte,
            },
        );

        assert_eq!(outcome.report.status, MigrationStatus::FailedKeepOldRoot);
        assert!(!outcome.report.verified);
        assert!(!outcome.report.errors.is_empty());
        assert!(
            outcome.report.errors.iter().any(|error| error.contains("SHA-256")),
            "应报告 SHA-256 不一致: {:?}",
            outcome.report.errors
        );
        // 本次启动使用旧根，旧数据一字未动。
        assert_eq!(outcome.paths.root, old_root.root);
        assert_eq!(snapshot(&old_root.root), old_before);

        // ERROR 日志行说明继续用旧根、下次重试；不写 INFO。
        let errors = outcome.report.error_lines();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("继续使用旧根"));
        assert!(errors[0].contains("下次启动重试"));
        assert!(outcome.report.info_lines().is_empty());

        // 新根不完整副本已回滚：没有 config.json / data 文件残留。
        assert!(!new_root.config_file.exists());
        assert!(!new_root.data_dir.exists());

        // 下次启动（故障消失）重试成功。
        let retry = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(retry.report.status, MigrationStatus::Migrated);
        assert_eq!(retry.report.file_count, 5);
        assert_eq!(retry.paths.root, new_root.root);
        assert_eq!(snapshot(&old_root.root), old_before);
    }

    /// 目标写入后 size 被追加 → size 校验失败 → 用旧根；旧数据保留。
    #[test]
    fn migrate_size_mismatch_falls_back_to_old_root() {
        let temp = TempDir::new("size-fail");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);
        let old_before = snapshot(&old_root.root);

        let outcome = resolve_with(
            base,
            Vec::new(),
            FaultInjection {
                tamper: TamperMode::AppendByte,
            },
        );

        assert_eq!(outcome.report.status, MigrationStatus::FailedKeepOldRoot);
        assert!(outcome.paths.root == old_root.root);
        assert!(
            outcome.report.errors.iter().any(|error| error.contains("size 不一致")),
            "应报告 size 不一致: {:?}",
            outcome.report.errors
        );
        assert_eq!(snapshot(&old_root.root), old_before);
        assert!(!new_root.data_dir.exists());
    }

    /// 目标目录写入失败（新根的 data 被一个文件占住）→ 用旧根；预存文件不被删除；修复后可重试。
    #[test]
    fn migrate_destination_write_failure_falls_back_and_keeps_new_root_files() {
        let temp = TempDir::new("dest-fail");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);
        let old_before = snapshot(&old_root.root);

        // 让新根 data 目标无法建目录（data 位置被文件占用；config.json/data 都不存在，仍满足迁移触发条件）。
        write_file(&new_root.data_dir, b"blocker");

        let outcome = resolve_with(base.clone(), Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::FailedKeepOldRoot);
        assert!(outcome.paths.root == old_root.root);
        assert_eq!(snapshot(&old_root.root), old_before);
        // 预先存在的阻塞文件不是本次迁移写入的，不得删除。
        assert!(new_root.data_dir.is_file());
        assert_eq!(fs::read(&new_root.data_dir).unwrap(), b"blocker");
        assert!(!new_root.config_file.exists());

        // 修复现场后重试成功。
        fs::remove_file(&new_root.data_dir).unwrap();
        let retry = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(retry.report.status, MigrationStatus::Migrated);
        assert!(retry.paths.root == new_root.root);
        assert_eq!(snapshot(&old_root.root), old_before);
    }

    // ---- 其他分支 ----------------------------------------------------------

    #[test]
    fn migrate_skipped_when_new_root_already_has_data() {
        let temp = TempDir::new("new-has-data");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);
        seed_new_root_with_data(&new_root);
        let new_before = snapshot(&new_root.root);
        let old_before = snapshot(&old_root.root);

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::SkippedNewRootHasData);
        assert!(outcome.report.status.uses_new_root());
        assert_eq!(outcome.paths.root, new_root.root);
        // 不覆盖新根，也不动旧根。
        assert_eq!(snapshot(&new_root.root), new_before);
        assert_eq!(snapshot(&old_root.root), old_before);
        let info = outcome.report.info_lines();
        assert_eq!(info.len(), 1);
        assert!(info[0].contains("跳过迁移"));
        assert!(info[0].contains("旧目录"));
        assert!(outcome.report.error_lines().is_empty());
    }

    #[test]
    fn migrate_skipped_when_new_root_has_data_dir_but_no_config() {
        let temp = TempDir::new("new-has-data-dir");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        seed_old_root(&old_root);
        write_file(&new_root.data_dir.join("2026-11-18.md"), "新根已有记录\n".as_bytes());

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::SkippedNewRootHasData);
        assert_eq!(outcome.paths.root, new_root.root);
        assert!(!old_root.config_file.exists() || old_root.config_file.is_file());
        assert!(!new_root.config_file.exists(), "不应从旧根补 config.json");
    }

    #[test]
    fn migrate_no_old_root_uses_new_root_and_creates_dirs() {
        let temp = TempDir::new("no-old");
        let base = temp.path().to_path_buf();
        let (new_root, _) = AppPaths::roots_from_base(&base);

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::NoOldRoot);
        assert!(outcome.report.status.uses_new_root());
        assert_eq!(outcome.paths.root, new_root.root);
        assert!(outcome.warnings.is_empty());
        for dir in [
            &outcome.paths.root,
            &outcome.paths.data_dir,
            &outcome.paths.output_dir,
            &outcome.paths.logs_dir,
        ] {
            assert!(dir.is_dir(), "{} 应已创建", dir.display());
        }
        let info = outcome.report.info_lines();
        assert_eq!(info.len(), 1);
        assert!(info[0].contains("未发现旧数据目录"));
        assert!(outcome.report.error_lines().is_empty());
    }

    #[test]
    fn migrate_empty_old_root_succeeds_with_zero_files() {
        let temp = TempDir::new("empty-old");
        let base = temp.path().to_path_buf();
        let (new_root, old_root) = AppPaths::roots_from_base(&base);
        fs::create_dir_all(&old_root.root).unwrap();

        let outcome = resolve_with(base, Vec::new(), FaultInjection::default());
        assert_eq!(outcome.report.status, MigrationStatus::Migrated);
        assert_eq!(outcome.report.file_count, 0);
        assert_eq!(outcome.report.total_bytes, 0);
        assert!(outcome.report.verified);
        assert_eq!(outcome.paths.root, new_root.root);
        assert!(outcome.report.info_lines()[0].contains("0 个文件"));
    }

    #[test]
    fn report_status_uses_new_root_except_on_failure() {
        assert!(MigrationStatus::Migrated.uses_new_root());
        assert!(MigrationStatus::SkippedNewRootHasData.uses_new_root());
        assert!(MigrationStatus::NoOldRoot.uses_new_root());
        assert!(!MigrationStatus::FailedKeepOldRoot.uses_new_root());
        assert_eq!(
            MigrationStatus::FailedKeepOldRoot.label(),
            "failed-keep-old-root"
        );
    }
}
