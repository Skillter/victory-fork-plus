use std::path::Path;

use anyhow::{Context, Result};

use crate::{Console, Unimplemented};

#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
) -> Result<()> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{BindReport, UnimplementedPolicy, WithTitleDlls};

    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    // The commit is baked into this crate only, and the window caption is drawn
    // by `victory-gpu`: hand it over before anything can draw one.
    victory_core::report::set_build(env!("VICTORY_COMMIT"), env!("VICTORY_COMMIT_SHORT"));
    let cfg = victory_core::Config::load()?;
    // Logging starts here, after the config has named the data directory
    // the log belongs in and before anything worth reading happens. A data
    // directory that cannot hold it (asset policy, permissions) leaves the
    // run on stderr rather than refusing to start it.
    let logs = match cfg.data_subdir("logs") {
        Ok(dir) => {
            victory_core::logging::init_run("info", &dir);
            Some(dir)
        }
        Err(_) => {
            victory_core::logging::init("info");
            None
        }
    };
    // The run's clock: the shutdown summary states how long the title ran,
    // and this is the first moment the log can mark as its start.
    victory_hle::modules::lifecycle::note_run_start();
    let begun = std::time::SystemTime::now();
    let started = std::time::Instant::now();
    // Every way out of this function that returns writes the footer; the ways
    // that end the process from inside the HLE cannot, and keep the `run
    // summary:` line of `crates/victory-hle/src/modules/lifecycle.rs`.
    let mut footer = Footer::new(started, begun, console, logs, &cfg);
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    let hash = prepared.original_hash.to_hex();
    match victory_game::identify(&hash) {
        Some(build) => println!("build          {} ({hash})", build.label),
        None => println!("build          unknown ({hash})"),
    }
    // The title's own facts are read here, before anything is mapped, so the
    // header states them at the top of the log instead of forty lines in.
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    let manifest_path = title_dir.join("MicrosoftGame.config");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    // A missing manifest leaves id 0 and no storage quota.
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    footer.title(title.id);
    emit_header(
        &cfg,
        &Header {
            exe,
            hash: &hash,
            title_id: title.id,
            manifest: manifest_path.is_file(),
            storage_mb: title.storage_bytes >> 20,
            console,
            begun,
        },
    );

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let image = victory_loader::load(&prepared, &images)?;
    // The program is mapped from the copy `load` wrote: the file's bytes
    // and their patched copy are not read again, and were held for the
    // whole run (two blocks of 114.7 MB each for the title, in
    // physical memory: run 65's memory map).
    drop(prepared);
    drop(original);
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    let show = |name: &str, report: BindReport| {
        println!(
            "bound          {name}: {} hle, {} host, {} title, {} missing",
            report.hle,
            report.host,
            report.title,
            report.missing.len()
        );
        for m in &report.missing {
            println!("  missing      {}!{}", m.dll, m.symbol);
        }
    };
    for (name, dll) in &dlls {
        show(name, dll.bind_imports(&with_dlls, policy)?);
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    show(&exe_name, image.bind_imports(&with_dlls, policy)?);
    if !execute {
        println!("not executing (pass --execute to start the title)");
        footer.stopped("prepared; not executed");
        return Ok(());
    }
    victory_loader::report_crashes();
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_hle::runtime::use_console_timers();
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    };
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file, show).map_err(|e| format!("{e:#}"))
    }));
    // SAFETY: imports are bound; from here on we run title code.
    let main = unsafe { Arc::new(image).spawn_main(dlls)? };
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    println!("title exited with {code}");
    footer.stopped(&format!("title exited with {code}"));
    Ok(())
}

/// The facts the header states, gathered by [`run`] where it learns them.
struct Header<'a> {
    exe: &'a Path,
    hash: &'a str,
    title_id: u32,
    manifest: bool,
    storage_mb: u64,
    console: Console,
    begun: std::time::SystemTime,
}

/// Writes the `RUN` block once, at the top of the log, so two runs of the same
/// binary can be compared by what they were given.
fn emit_header(cfg: &victory_core::Config, header: &Header<'_>) {
    let program = format!("victory {}", env!("CARGO_PKG_VERSION"));
    let run = victory_core::report::RunHeader {
        program,
        build: env!("VICTORY_COMMIT_SHORT").to_owned(),
        commit: env!("VICTORY_COMMIT").to_owned(),
        branch: env!("VICTORY_BRANCH").to_owned(),
        dirty: env!("VICTORY_DIRTY").to_owned(),
        started_utc: victory_core::logging::utc_text(header.begun),
        started_local: victory_core::logging::local_text(),
        filter: victory_core::logging::filter_text().to_owned(),
        cap: victory_core::logging::cap_text().to_owned(),
        log_file: victory_core::logging::current_log_file().map(Path::to_path_buf),
        config_file: cfg.source.clone(),
        game_dir: cfg.game_dir.clone(),
        exe: header.exe.to_path_buf(),
        exe_hash: header.hash.to_owned(),
        title_id: header.title_id,
        manifest: header.manifest,
        storage_mb: header.storage_mb,
        console: console_name(header.console).to_owned(),
        cache_dir: cfg.cache_dir.clone(),
        data_dir: cfg.data_dir.clone(),
        env: victory_env(),
    };
    victory_core::logging::emit("INFO", &run.render());
}

/// The console the header, the footer and the history row name.
fn console_name(console: Console) -> &'static str {
    match console {
        Console::SeriesX => "series-x",
        Console::SeriesS => "series-s",
    }
}

/// Every `VICTORY_*` variable that is set, as the user set it.
///
/// Only that prefix: a log may say what was asked of it, never what else this
/// machine has in its environment.
fn victory_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            name.starts_with("VICTORY_")
                .then(|| (name, value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Bytes under `path`; a missing directory is 0.
fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// The mean of one observed metric over the run, in the unit its name carries.
///
/// `None` when nothing measured it, so the history row leaves the column empty
/// instead of stating a zero that would average into the next run.
fn mean(meter: &victory_core::meter::Snapshot, name: &str) -> Option<f64> {
    let metric = meter.metrics.iter().find(|metric| metric.name == name)?;
    (metric.count > 0).then(|| metric.sum as f64 / metric.count as f64)
}

/// Writes the `STOP` block on every way out of [`run`], and installs the
/// renderer the paths that end the process from inside the HLE use.
///
/// `lifecycle::end` (a closed window, the suspend deadline) terminates the
/// process rather than returning, so this frame is gone by then; it writes the
/// block from `crates/victory-hle/src/modules/lifecycle.rs` (`last_records`).
/// That path knows no directories, so the cache sizes are installed here, at
/// start-up, through [`victory_core::report::set_footer_hook`]: both paths
/// render through [`victory_core::report::stop_block`], which writes the block
/// once per process whichever of them is first.
///
/// The frame figures are the two metrics the presentation path records:
/// `frames_n`, counted once per presented frame by `victory_gpu::queue::present`,
/// and `frame_ms`, observed once per frame by `victory_gpu::host::Meter::frame`.
/// A run that never drew has neither and prints no fps.
///
/// It also writes the run's row of `<log_dir>/runs.tsv`
/// ([`victory_core::report::append_run`]) on the same path, from the same
/// figures: a log is a megabyte per run and a year of them cannot be read
/// across, while one row per run answers "did this commit get slower".
struct Footer {
    started: std::time::Instant,
    reason: String,
}

/// The title the footer names. A static because the hook that renders it may
/// run on the thread `lifecycle::end` spawned, after this frame is gone.
static TITLE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Footer {
    fn new(
        started: std::time::Instant,
        begun: std::time::SystemTime,
        console: Console,
        logs: Option<std::path::PathBuf>,
        cfg: &victory_core::Config,
    ) -> Self {
        let (cache_dir, data_dir) = (cfg.cache_dir.clone(), cfg.data_dir.clone());
        let console = console_name(console);
        victory_core::report::set_footer_hook(Box::new(move |reason, duration| {
            let mut footer = victory_core::report::RunFooter::from_counters(reason, duration);
            let title = format!("{:08X}", TITLE_ID.load(std::sync::atomic::Ordering::Relaxed));
            // A directory that is not there yet is 0 bytes, not an error: the
            // footer is written on every path, including the ones that failed
            // before they could make one.
            footer.cache = vec![
                ("shaders".to_owned(), dir_size(&cache_dir.join("shaders"))),
                ("pipelines".to_owned(), dir_size(&cache_dir.join("pipelines"))),
                ("images".to_owned(), dir_size(&cache_dir.join("images"))),
                ("storage".to_owned(), dir_size(&data_dir.join(&title).join("storage"))),
                ("temp".to_owned(), dir_size(&data_dir.join(&title).join("temp"))),
            ];
            // The history row: the same figures the block states, stamped with
            // the commit and the minute the run ended. Written before the block
            // is emitted, so a run that dies writing the footer has still left
            // its row, and a row that cannot be written is a warning, never a
            // failed run.
            let record = victory_core::report::RunRecord {
                started_utc: victory_core::logging::utc_text(begun),
                ended_utc: victory_core::logging::utc_text(std::time::SystemTime::now()),
                commit: env!("VICTORY_COMMIT").to_owned(),
                branch: env!("VICTORY_BRANCH").to_owned(),
                dirty: env!("VICTORY_DIRTY").to_owned(),
                console: console.to_owned(),
                duration,
                frames: footer.frames,
                // The worst frame the meter saw, and the two figures only its
                // five-second line prints: the mean is read from the totals.
                worst_frame_ms: footer.worst_frame_ms,
                draws_mean: mean(&footer.meter, "draws_frame"),
                gpu_wait_ms: mean(&footer.meter, "gpu_wait_us").map(|us| us / 1_000.0),
                reason: reason.to_owned(),
                log: victory_core::logging::current_log_file().map(Path::to_path_buf),
            };
            if let Some(dir) = &logs
                && let Err(error) = victory_core::report::append_run(
                    &dir.join(victory_core::report::RUNS_FILE),
                    &record,
                )
            {
                victory_core::logging::emit(
                    "WARN",
                    &format!("run history left out of {}: {error}", dir.display()),
                );
            }
            footer.render()
        }));
        Self {
            started,
            reason: "the run stopped before the title ended".to_owned(),
        }
    }

    fn title(&mut self, id: u32) {
        TITLE_ID.store(id, std::sync::atomic::Ordering::Relaxed);
    }

    fn stopped(&mut self, reason: &str) {
        self.reason = reason.to_owned();
    }
}

impl Drop for Footer {
    fn drop(&mut self) {
        // The same call `lifecycle::end` makes, through the same guard: a run
        // that returned and a run that was terminated cannot both write it.
        if let Some(block) = victory_core::report::stop_block(&self.reason, self.started.elapsed()) {
            victory_core::logging::emit("INFO", &block);
        }
    }
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// DLLs the host or our runtime provide are never taken from the title.
#[cfg(windows)]
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. The title's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    fn load(
        &self,
        file: &str,
        show: impl Fn(&str, victory_loader::BindReport),
    ) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    show(name, dll.bind_imports(&resolver, self.policy)?);
                    set_image_path(dll.base(), &self.title_dir.join(name));
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[cfg(windows)]
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

#[cfg(windows)]
impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }
}

#[cfg(not(windows))]
pub fn run(_: &Path, _: Unimplemented, _: bool, _: bool, _: Console) -> Result<()> {
    anyhow::bail!("running titles requires Windows")
}
