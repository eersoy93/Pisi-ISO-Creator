//! Slint user interface controller.

use crate::command::{Event, EventSink, Executor, Plan};
use crate::pipeline::{build_plan, edit_plan};
use crate::pisi::{self, RepoIndex};
use crate::project::{
    EditJob, Project, format_list, format_repositories, parse_list, parse_repositories,
};
use slint::{ComponentHandle, ModelRc, SharedString, StandardListViewItem, VecModel};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

slint::include_modules!();

/// Maximum number of log lines kept in the log view.
const MAX_LOG_LINES: usize = 3000;
const MAX_SEARCH_RESULTS: usize = 500;

#[derive(Default)]
struct Shared {
    /// Events produced by the worker thread, drained by a UI timer.
    events: Mutex<Vec<Event>>,
    index: Mutex<Option<RepoIndex>>,
}

struct UiState {
    log: VecDeque<String>,
    /// Package names shown in the search result list.
    results: Vec<String>,
    cancel: Arc<AtomicBool>,
}

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let shared = Arc::new(Shared::default());
    let state = Rc::new(RefCell::new(UiState {
        log: VecDeque::new(),
        results: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    }));

    project_to_ui(&ui, &Project::default());
    edit_job_to_ui(&ui, &EditJob::default());
    if let Some(repo) = Project::default().repositories.first() {
        ui.set_index_source(repo.url.clone().into());
    }

    // ---- Project handling ----
    let weak = ui.as_weak();
    ui.on_new_project(move || {
        let ui = weak.unwrap();
        project_to_ui(&ui, &Project::default());
        ui.set_status_text("New project created with default settings.".into());
    });

    let weak = ui.as_weak();
    ui.on_open_project(move || {
        let ui = weak.unwrap();
        let path = PathBuf::from(ui.get_project_path().as_str());
        match Project::load(&path) {
            Ok(p) => {
                project_to_ui(&ui, &p);
                ui.set_status_text(format!("Opened {}.", path.display()).into());
            }
            Err(e) => ui.set_status_text(format!("Error: {e}").into()),
        }
    });

    let weak = ui.as_weak();
    ui.on_save_project(move || {
        let ui = weak.unwrap();
        let path = PathBuf::from(ui.get_project_path().as_str());
        let result = ui_to_project(&ui).and_then(|p| p.save(&path));
        ui.set_status_text(match result {
            Ok(()) => format!("Saved {}.", path.display()).into(),
            Err(e) => format!("Error: {e}").into(),
        });
    });

    let weak = ui.as_weak();
    ui.on_validate_project(move || {
        let ui = weak.unwrap();
        let text = match ui_to_project(&ui).map(|p| p.validate()) {
            Ok(errors) if errors.is_empty() => "Project is valid.".to_string(),
            Ok(errors) => format!("Project has problems:\n• {}", errors.join("\n• ")),
            Err(e) => format!("Error: {e}"),
        };
        ui.set_status_text(text.into());
    });

    let (weak, st) = (ui.as_weak(), state.clone());
    ui.on_show_plan(move || {
        let ui = weak.unwrap();
        match checked_project(&ui) {
            Ok(p) => append_log(&ui, &st, &build_plan(&p).describe()),
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    let (weak, st, sh) = (ui.as_weak(), state.clone(), shared.clone());
    ui.on_build_iso(move || {
        let ui = weak.unwrap();
        match checked_project(&ui) {
            Ok(p) => start_plan(&ui, &st, &sh, build_plan(&p)),
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // ---- Edit jobs ----
    let weak = ui.as_weak();
    ui.on_load_edit_job(move || {
        let ui = weak.unwrap();
        let path = PathBuf::from(ui.get_edit_job_path().as_str());
        match EditJob::load(&path) {
            Ok(job) => {
                edit_job_to_ui(&ui, &job);
                ui.set_status_text(format!("Opened {}.", path.display()).into());
            }
            Err(e) => ui.set_status_text(format!("Error: {e}").into()),
        }
    });

    let weak = ui.as_weak();
    ui.on_save_edit_job(move || {
        let ui = weak.unwrap();
        let path = PathBuf::from(ui.get_edit_job_path().as_str());
        let result = ui_to_edit_job(&ui).and_then(|j| j.save(&path));
        ui.set_status_text(match result {
            Ok(()) => format!("Saved {}.", path.display()).into(),
            Err(e) => format!("Error: {e}").into(),
        });
    });

    let (weak, st) = (ui.as_weak(), state.clone());
    ui.on_show_edit_plan(move || {
        let ui = weak.unwrap();
        match checked_edit_job(&ui) {
            Ok(job) => append_log(&ui, &st, &edit_plan(&job).describe()),
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    let (weak, st, sh) = (ui.as_weak(), state.clone(), shared.clone());
    ui.on_edit_iso(move || {
        let ui = weak.unwrap();
        match checked_edit_job(&ui) {
            Ok(job) => start_plan(&ui, &st, &sh, edit_plan(&job)),
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // ---- Run control ----
    let st = state.clone();
    ui.on_cancel(move || st.borrow().cancel.store(true, Ordering::SeqCst));

    let (weak, st) = (ui.as_weak(), state.clone());
    ui.on_clear_log(move || {
        st.borrow_mut().log.clear();
        weak.unwrap().set_log_text(SharedString::new());
    });

    // ---- Repository browser ----
    let (weak, sh) = (ui.as_weak(), shared.clone());
    ui.on_load_index(move || {
        let ui = weak.unwrap();
        let source = ui.get_index_source().trim().to_string();
        if source.is_empty() {
            ui.set_index_status("Enter a pisi-index.xml(.xz) URL or path!".into());
            return;
        }
        ui.set_index_loading(true);
        ui.set_index_status(format!("Loading {source}…").into());
        let (weak, sh) = (ui.as_weak(), sh.clone());
        std::thread::spawn(move || {
            let result = pisi::load_index(&source);
            let status = match &result {
                Ok(index) => format!(
                    "{} packages and {} components in {source}.",
                    index.packages.len(),
                    index.components.len()
                ),
                Err(e) => format!("Error: {e}"),
            };
            if let Ok(index) = result {
                *sh.index.lock().unwrap() = Some(index);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_index_loading(false);
                ui.set_index_status(status.into());
                let query = ui.get_search_text();
                ui.invoke_search(query);
            });
        });
    });

    let (weak, st, sh) = (ui.as_weak(), state.clone(), shared.clone());
    ui.on_search(move |query| {
        let ui = weak.unwrap();
        let guard = sh.index.lock().unwrap();
        let Some(index) = guard.as_ref() else { return };
        let found = index.search(&query);
        let total = found.len();
        let found = &found[..total.min(MAX_SEARCH_RESULTS)];
        st.borrow_mut().results = found.iter().map(|p| p.name.clone()).collect();
        let items: Vec<StandardListViewItem> = found
            .iter()
            .map(|p| StandardListViewItem::from(SharedString::from(p.label())))
            .collect();
        ui.set_search_results(ModelRc::new(VecModel::from(items)));
        ui.set_selected_result(-1);
        ui.set_package_details(if total > MAX_SEARCH_RESULTS {
            format!("Showing {MAX_SEARCH_RESULTS} of {total} matches; refine the search!").into()
        } else {
            format!("{total} matching package(s).").into()
        });
    });

    let (weak, st, sh) = (ui.as_weak(), state.clone(), shared.clone());
    ui.on_select_package(move |i| {
        let ui = weak.unwrap();
        let name = usize::try_from(i)
            .ok()
            .and_then(|i| st.borrow().results.get(i).cloned());
        let guard = sh.index.lock().unwrap();
        if let (Some(name), Some(index)) = (name, guard.as_ref())
            && let Some(p) = index.packages.iter().find(|p| p.name == name)
        {
            ui.set_package_details(p.details().into());
        }
    });

    let (weak, st) = (ui.as_weak(), state.clone());
    ui.on_add_selected(move |target| {
        let ui = weak.unwrap();
        let Some(name) = usize::try_from(ui.get_selected_result())
            .ok()
            .and_then(|i| st.borrow().results.get(i).cloned())
        else {
            return;
        };
        let (current, set): (SharedString, fn(&AppWindow, SharedString)) = match target.as_str() {
            "excluded" => (ui.get_excluded_packages(), AppWindow::set_excluded_packages),
            "edit-install" => (ui.get_edit_install(), AppWindow::set_edit_install),
            _ => (ui.get_packages(), AppWindow::set_packages),
        };
        let mut list = parse_list(&current);
        if list.contains(&name) {
            ui.set_status_text(format!("'{name}' is already in the list!").into());
            return;
        }
        list.push(name.clone());
        set(&ui, format_list(&list).into());
        ui.set_status_text(format!("Added '{name}'.").into());
    });

    // Drains worker events into the UI.
    let timer = slint::Timer::default();
    let (weak, st, sh) = (ui.as_weak(), state.clone(), shared.clone());
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let events = std::mem::take(&mut *sh.events.lock().unwrap());
            if events.is_empty() {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            let mut lines = Vec::new();
            for event in events {
                match event {
                    Event::Log(line) => lines.push(line),
                    Event::Progress {
                        step,
                        total,
                        description,
                    } => {
                        ui.set_progress(if total == 0 {
                            1.0
                        } else {
                            step as f32 / total as f32
                        });
                        ui.set_status_text(
                            format!("[{}/{total}] {description}", (step + 1).min(total)).into(),
                        );
                    }
                    Event::Finished(result) => {
                        ui.set_running(false);
                        ui.set_status_text(match result {
                            Ok(()) => "Finished successfully.".into(),
                            Err(e) => format!("Failed: {e}").into(),
                        });
                    }
                }
            }
            if !lines.is_empty() {
                append_log(&ui, &st, &lines.join("\n"));
            }
        },
    );

    ui.run()
}

fn append_log(ui: &AppWindow, state: &Rc<RefCell<UiState>>, text: &str) {
    let mut st = state.borrow_mut();
    st.log.extend(text.lines().map(str::to_owned));
    let excess = st.log.len().saturating_sub(MAX_LOG_LINES);
    st.log.drain(..excess);
    let joined = st
        .log
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    ui.set_log_text(joined.into());
}

fn start_plan(ui: &AppWindow, state: &Rc<RefCell<UiState>>, shared: &Arc<Shared>, plan: Plan) {
    if ui.get_running() {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    state.borrow_mut().cancel = cancel.clone();
    ui.set_running(true);
    ui.set_progress(0.0);
    ui.set_status_text(format!("Running: {}", plan.title).into());
    let dry_run = ui.get_dry_run();
    let sh = shared.clone();
    let sink: EventSink = Arc::new(move |e| sh.events.lock().unwrap().push(e));
    std::thread::spawn(move || {
        let _ = Executor::new(dry_run, cancel, sink).execute(&plan);
    });
}

fn checked_project(ui: &AppWindow) -> Result<Project, String> {
    let project = ui_to_project(ui).map_err(|e| format!("Error: {e}"))?;
    let errors = project.validate();
    if errors.is_empty() {
        Ok(project)
    } else {
        Err(format!("Project has problems:\n• {}", errors.join("\n• ")))
    }
}

fn checked_edit_job(ui: &AppWindow) -> Result<EditJob, String> {
    let job = ui_to_edit_job(ui).map_err(|e| format!("Error: {e}"))?;
    let errors = job.validate();
    if errors.is_empty() {
        Ok(job)
    } else {
        Err(format!("Edit job has problems:\n• {}", errors.join("\n• ")))
    }
}

fn project_to_ui(ui: &AppWindow, p: &Project) {
    ui.set_distro_name(p.name.clone().into());
    ui.set_distro_version(p.version.clone().into());
    ui.set_volume_label(p.volume_label.clone().into());
    ui.set_hostname(p.hostname.clone().into());
    ui.set_live_user(p.live_user.clone().into());
    ui.set_work_dir(p.work_dir.display().to_string().into());
    ui.set_output_iso(p.output_iso.display().to_string().into());
    ui.set_compression(p.squashfs_compression.clone().into());
    ui.set_kernel_cmdline(p.kernel_cmdline.clone().into());
    ui.set_live_initramfs(p.live_initramfs);
    ui.set_post_install_script(p.post_install_script.clone().into());
    ui.set_repositories(format_repositories(&p.repositories).into());
    ui.set_components(format_list(&p.components).into());
    ui.set_packages(format_list(&p.packages).into());
    ui.set_excluded_packages(format_list(&p.excluded_packages).into());
}

fn ui_to_project(ui: &AppWindow) -> Result<Project, String> {
    Ok(Project {
        name: ui.get_distro_name().trim().to_string(),
        version: ui.get_distro_version().trim().to_string(),
        volume_label: ui.get_volume_label().trim().to_string(),
        hostname: ui.get_hostname().trim().to_string(),
        live_user: ui.get_live_user().trim().to_string(),
        repositories: parse_repositories(&ui.get_repositories())?,
        components: parse_list(&ui.get_components()),
        packages: parse_list(&ui.get_packages()),
        excluded_packages: parse_list(&ui.get_excluded_packages()),
        work_dir: PathBuf::from(ui.get_work_dir().trim()),
        output_iso: PathBuf::from(ui.get_output_iso().trim()),
        squashfs_compression: ui.get_compression().to_string(),
        kernel_cmdline: ui.get_kernel_cmdline().trim().to_string(),
        live_initramfs: ui.get_live_initramfs(),
        post_install_script: ui.get_post_install_script().to_string(),
    })
}

fn edit_job_to_ui(ui: &AppWindow, job: &EditJob) {
    ui.set_edit_input_iso(job.input_iso.display().to_string().into());
    ui.set_edit_output_iso(job.output_iso.display().to_string().into());
    ui.set_edit_work_dir(job.work_dir.display().to_string().into());
    ui.set_edit_volume_label(job.volume_label.clone().into());
    ui.set_edit_repositories(format_repositories(&job.repositories).into());
    ui.set_edit_upgrade(job.upgrade);
    ui.set_edit_install(format_list(&job.install_packages).into());
    ui.set_edit_remove(format_list(&job.remove_packages).into());
    ui.set_edit_compression(job.squashfs_compression.clone().into());
    ui.set_edit_script(job.post_install_script.clone().into());
}

fn ui_to_edit_job(ui: &AppWindow) -> Result<EditJob, String> {
    Ok(EditJob {
        input_iso: PathBuf::from(ui.get_edit_input_iso().trim()),
        output_iso: PathBuf::from(ui.get_edit_output_iso().trim()),
        work_dir: PathBuf::from(ui.get_edit_work_dir().trim()),
        volume_label: ui.get_edit_volume_label().trim().to_string(),
        repositories: parse_repositories(&ui.get_edit_repositories())?,
        upgrade: ui.get_edit_upgrade(),
        install_packages: parse_list(&ui.get_edit_install()),
        remove_packages: parse_list(&ui.get_edit_remove()),
        squashfs_compression: ui.get_edit_compression().to_string(),
        post_install_script: ui.get_edit_script().to_string(),
    })
}
