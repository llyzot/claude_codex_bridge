use std::collections::HashSet;
use std::env;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::prelude::{Color, Frame, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::args::Args;
use crate::client::CcbdClient;
use crate::model::{
    AgentView, CommsItem, ProjectView, ProjectViewResponse, RowTarget, SidebarViewInfo, WindowView,
    row_targets,
};
use crate::status::{activity_color_with_theme, activity_symbol};
use crate::theme::{RuntimeThemeResolver, SidebarTheme};

const PROJECT_VIEW_REFRESH_MIN_MS: u64 = 100;
const PROJECT_VIEW_REFRESH_MAX_MS: u64 = 5000;
const PROJECT_VIEW_REFRESH_DEFAULT_MS: u64 = 1000;
const DEFAULT_TREE_HEIGHT_PERCENT: u16 = 50;
const DEFAULT_COMMS_HEIGHT_PERCENT: u16 = 15;
const DEFAULT_TIPS_HEIGHT_PERCENT: u16 = 35;
const TREE_CONTROL_CONTENT_WIDTH: u16 = 3;
const TREE_SETTINGS_SYMBOL: &str = "⚙";
const TREE_KILL_SYMBOL: &str = "×";
const SIDEBAR_CCB_BIN_ENV: &str = "CCB_SIDEBAR_CCB_BIN";
const COMMS_ACTION_RETRY_COLS: std::ops::RangeInclusive<u16> = 0..=1;
const COMMS_ACTION_CANCEL_COLS: std::ops::RangeInclusive<u16> = 3..=4;
const COMMS_ACTION_CLEAR_COLS: std::ops::RangeInclusive<u16> = 6..=7;
const COMMS_DETAIL_INDENT_WIDTH: usize = 3;
const MIN_SECTION_HEIGHT: u16 = 3;

pub fn run(args: Args) -> io::Result<()> {
    let action = run_tui(&args)?;
    match action {
        ExitAction::SidebarOnly => {}
        ExitAction::KillProject => {
            if args.native_state_root.is_some() {
                CcbdClient::from_args(&args)
                    .and_then(|client| client.stop_native())
                    .map_err(io::Error::other)?;
            } else {
                run_ccb_kill(&args.project_root)?;
            }
        }
    }
    Ok(())
}

fn run_tui(args: &Args) -> io::Result<ExitAction> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    if let Err(err) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(err);
    }
    let _session = TuiSession;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let client = CcbdClient::from_args(args).map_err(io::Error::other)?;
    let ccb_program = ccb_program();
    let mut theme_resolver = RuntimeThemeResolver::new(&args.theme);
    let mut app = SidebarApp::with_theme(args.pane_window.clone(), theme_resolver.theme());

    loop {
        if app.needs_refresh() {
            app.set_theme(theme_resolver.refresh());
            match client.project_view() {
                Ok(response) => app.apply_response(response),
                Err(err) => app.set_error(err),
            }
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(ExitAction::SidebarOnly),
                    KeyCode::Char('Q') => return Ok(ExitAction::KillProject),
                    KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
                    KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
                    KeyCode::Char('r') => app.restart_panes(&client),
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.force_refresh()
                    }
                    KeyCode::Char('R') => app.recover_first_visible_comms(&client),
                    KeyCode::Enter => app.focus_selected_target(&client),
                    KeyCode::Tab => app.focus_pane_window(&client),
                    _ => {}
                },
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(action) = app.handle_mouse_down(
                                mouse.column,
                                mouse.row,
                                area,
                                &client,
                                &args.project_root,
                                &ccb_program,
                            ) {
                                return Ok(action);
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            app.handle_mouse_drag(mouse.column, mouse.row, area);
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            app.handle_mouse_up();
                        }
                        MouseEventKind::ScrollDown => {
                            app.scroll_panel_at(mouse.column, mouse.row, area, 1);
                        }
                        MouseEventKind::ScrollUp => {
                            app.scroll_panel_at(mouse.column, mouse.row, area, -1);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitAction {
    SidebarOnly,
    KillProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarResizeDivider {
    TreeComms,
    CommsTips,
}

fn run_ccb_kill(project_root: &Path) -> io::Result<()> {
    run_ccb_kill_with_program(ccb_program(), project_root)
}

fn run_ccb_kill_with_program(program: PathBuf, project_root: &Path) -> io::Result<()> {
    let status = Command::new(program)
        .arg("kill")
        .current_dir(project_root)
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "ccb kill failed with status {status}"
    )))
}

fn ccb_program() -> PathBuf {
    if let Some(program) = env::var_os(SIDEBAR_CCB_BIN_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return program;
    }
    env::current_exe()
        .ok()
        .and_then(|path| ccb_program_for_sidebar(&path))
        .unwrap_or_else(|| PathBuf::from("ccb"))
}

fn launch_config_ui(project_root: &Path, program: &Path) -> io::Result<ConfigUiLaunch> {
    let mut child = config_ui_command(project_root, program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("config ui stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("config ui stderr pipe unavailable"))?;
    let launch = Arc::new(Mutex::new(ConfigUiLaunchStatus::Opening));
    let worker_launch = Arc::clone(&launch);
    thread::spawn(move || monitor_config_ui(child, stdout, stderr, worker_launch));
    Ok(launch)
}

fn config_ui_command(project_root: &Path, program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .arg("--project")
        .arg(project_root)
        .arg("config")
        .arg("ui")
        .current_dir(project_root);
    command
}

fn ccb_program_for_sidebar(sidebar_exe: &Path) -> Option<PathBuf> {
    for ancestor in sidebar_exe.ancestors().skip(1).take(8) {
        let candidate = ancestor.join("ccb");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigUiLaunchStatus {
    Opening,
    Ready(String),
    Manual(String),
    Failed(String),
}

type ConfigUiLaunch = Arc<Mutex<ConfigUiLaunchStatus>>;

fn monitor_config_ui(
    mut child: std::process::Child,
    stdout: impl Read,
    stderr: impl Read + Send + 'static,
    launch: ConfigUiLaunch,
) {
    let stderr_text = Arc::new(Mutex::new(String::new()));
    let stderr_result = Arc::clone(&stderr_text);
    let stderr_worker = thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut text);
        if let Ok(mut target) = stderr_result.lock() {
            *target = text;
        }
    });
    let mut reported_url = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("url: ")
            && !url.trim().is_empty()
        {
            let url = url.trim().to_string();
            reported_url = Some(url.clone());
            set_config_ui_launch_status(&launch, ConfigUiLaunchStatus::Ready(url));
        } else if line.starts_with("browser_open: failed")
            && let Some(url) = reported_url.as_ref()
        {
            set_config_ui_launch_status(&launch, ConfigUiLaunchStatus::Manual(url.clone()));
        }
    }
    let result = child.wait();
    let _ = stderr_worker.join();
    let stderr = stderr_text
        .lock()
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "config ui stderr unavailable".to_string());
    match result {
        Ok(status) if status.success() && reported_url.is_some() => {
            set_config_ui_launch_status(
                &launch,
                ConfigUiLaunchStatus::Failed("session closed; click to reopen".to_string()),
            );
        }
        Ok(status) if status.success() => {
            set_config_ui_launch_status(
                &launch,
                ConfigUiLaunchStatus::Failed("exited before reporting a URL".to_string()),
            );
        }
        Ok(status) => {
            let detail = if stderr.is_empty() {
                format!("exited with status {status}")
            } else {
                compact_launch_error(&stderr)
            };
            set_config_ui_launch_status(&launch, ConfigUiLaunchStatus::Failed(detail));
        }
        Err(err) => set_config_ui_launch_status(
            &launch,
            ConfigUiLaunchStatus::Failed(format!("wait failed: {err}")),
        ),
    }
}

fn set_config_ui_launch_status(launch: &ConfigUiLaunch, status: ConfigUiLaunchStatus) {
    if let Ok(mut current) = launch.lock() {
        *current = status;
    }
}

fn compact_launch_error(error: &str) -> String {
    let one_line = error.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(180).collect()
}

struct TuiSession;

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[derive(Debug, Clone)]
pub struct SidebarApp {
    pane_window: String,
    response: Option<ProjectViewResponse>,
    last_error: Option<String>,
    failure_count: u32,
    selected: usize,
    selected_comms: Option<usize>,
    hidden_comms: HashSet<String>,
    tree_scroll: usize,
    comms_scroll: usize,
    tips_scroll: usize,
    section_heights: Option<(u16, u16, u16)>,
    dragging_divider: Option<SidebarResizeDivider>,
    selection_follows_focus: bool,
    refresh_after: Instant,
    theme: SidebarTheme,
    config_ui_launch: Option<ConfigUiLaunch>,
}

impl SidebarApp {
    pub fn new(pane_window: String) -> Self {
        Self::with_theme(pane_window, SidebarTheme::default_dark())
    }

    pub fn with_theme(pane_window: String, theme: SidebarTheme) -> Self {
        Self {
            pane_window,
            response: None,
            last_error: None,
            failure_count: 0,
            selected: 0,
            selected_comms: None,
            hidden_comms: HashSet::new(),
            tree_scroll: 0,
            comms_scroll: 0,
            tips_scroll: 0,
            section_heights: None,
            dragging_divider: None,
            selection_follows_focus: true,
            refresh_after: Instant::now(),
            theme,
            config_ui_launch: None,
        }
    }

    fn theme(&self) -> SidebarTheme {
        self.theme
    }

    fn set_theme(&mut self, theme: SidebarTheme) {
        self.theme = theme;
    }

    pub fn apply_response(&mut self, response: ProjectViewResponse) {
        self.response = Some(response);
        self.last_error = None;
        self.failure_count = 0;
        if self.selection_follows_focus {
            self.select_active_target();
        } else {
            self.clamp_selection();
        }
        self.refresh_after = Instant::now() + self.refresh_interval();
    }

    pub fn set_error(&mut self, error: String) {
        self.last_error = Some(error);
        self.failure_count = self.failure_count.saturating_add(1);
        self.refresh_after = Instant::now() + refresh_backoff_for_failures(self.failure_count);
    }

    pub fn force_refresh(&mut self) {
        self.refresh_after = Instant::now();
    }

    pub fn restart_panes(&mut self, client: &CcbdClient) {
        match client.restart_panes() {
            Ok(()) => self.force_refresh(),
            Err(err) => {
                self.set_error(err);
                self.force_refresh();
            }
        }
    }

    pub fn needs_refresh(&self) -> bool {
        Instant::now() >= self.refresh_after
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.targets().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, (count - 1) as isize);
        self.selected = next as usize;
        self.selection_follows_focus = false;
    }

    pub fn selected_target(&self) -> Option<RowTarget> {
        self.targets().get(self.selected).cloned()
    }

    pub fn namespace_epoch(&self) -> Option<u64> {
        self.view().and_then(|view| view.namespace.epoch)
    }

    pub fn focus_selected_target(&mut self, client: &CcbdClient) {
        if let Some(target) = self.selected_target() {
            self.selection_follows_focus = true;
            self.focus_target(client, target);
        }
    }

    pub fn focus_pane_window(&mut self, client: &CcbdClient) {
        self.selection_follows_focus = true;
        self.focus_target(client, RowTarget::Window(self.pane_window.clone()));
    }

    pub fn focus_target_at(&mut self, column: u16, row: u16, area: Rect, client: &CcbdClient) {
        let Some(index) = self.target_index_at(column, row, area) else {
            return;
        };
        self.selected = index;
        self.selection_follows_focus = true;
        self.focus_selected_target(client);
    }

    fn handle_mouse_down(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
        client: &CcbdClient,
        project_root: &Path,
        ccb_program: &Path,
    ) -> Option<ExitAction> {
        let areas = self.sidebar_areas(area);
        match header_action_at(column, row, areas.tree) {
            Some(HeaderMouseAction::Settings) => {
                if client.is_native() {
                    self.set_error(
                        "Configuration editing is not available in this native view yet".into(),
                    );
                    return None;
                }
                if !self.config_ui_launch_is_active() {
                    match launch_config_ui(project_root, ccb_program) {
                        Ok(launch) => self.config_ui_launch = Some(launch),
                        Err(err) => self.set_error(format!("config ui launch failed: {err}")),
                    }
                }
                return None;
            }
            Some(HeaderMouseAction::KillProject) => return Some(ExitAction::KillProject),
            None => {}
        }
        if self.start_resize_at(column, row, area) {
            return None;
        }
        if self.handle_comms_mouse_down(column, row, area, client) {
            return None;
        }
        self.focus_target_at(column, row, area, client);
        None
    }

    fn handle_mouse_drag(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some(divider) = self.dragging_divider else {
            return false;
        };
        self.drag_resize_to(divider, column, row, area)
    }

    fn handle_mouse_up(&mut self) {
        self.dragging_divider = None;
    }

    pub fn recover_first_visible_comms(&mut self, client: &CcbdClient) {
        let Some(item) = self
            .visible_comms_limited()
            .into_iter()
            .find(|item| item.recoverable)
        else {
            return;
        };
        self.recover_comms_item(client, &item);
    }

    pub fn recover_comms_at(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
        client: &CcbdClient,
    ) -> bool {
        self.handle_comms_mouse_down(column, row, area, client)
    }

    pub fn handle_comms_mouse_down(
        &mut self,
        column: u16,
        row: u16,
        area: Rect,
        client: &CcbdClient,
    ) -> bool {
        let Some((index, action)) = self.comms_action_at(column, row, area) else {
            return false;
        };
        let Some(item) = self.visible_comms_limited().get(index).cloned() else {
            return false;
        };
        self.selected_comms = Some(index);
        match action {
            CommsMouseAction::Retry if item.recoverable => self.recover_comms_item(client, &item),
            CommsMouseAction::Cancel if comms_cancel_enabled(&item) => {
                self.cancel_comms_item(client, &item)
            }
            CommsMouseAction::Clear => self.dismiss_comms_item(client, &item),
            _ => {}
        }
        true
    }

    fn scroll_panel_at(&mut self, column: u16, row: u16, area: Rect, delta: isize) -> bool {
        let areas = self.sidebar_areas(area);
        if rect_contains(areas.tree, column, row) {
            self.scroll_tree(delta, areas.tree.height.saturating_sub(2));
            return true;
        }
        if rect_contains(areas.comms, column, row) {
            self.scroll_comms(delta, areas.comms);
            return true;
        }
        if let Some(tips) = areas.tips
            && rect_contains(tips, column, row)
        {
            self.scroll_tips(delta, tips.height.saturating_sub(2));
            return true;
        }
        false
    }

    fn scroll_tree(&mut self, delta: isize, viewport_height: u16) {
        let max_scroll = tree_scroll_max_for_len(tree_rows(self).len(), viewport_height);
        let current = self.tree_scroll.min(max_scroll);
        self.tree_scroll = offset_after_scroll(current, delta, max_scroll);
    }

    fn scroll_comms(&mut self, delta: isize, area: Rect) {
        let max_scroll = self.comms_scroll_max(area);
        let current = self.comms_scroll.min(max_scroll);
        self.comms_scroll = offset_after_scroll(current, delta, max_scroll);
    }

    fn scroll_tips(&mut self, delta: isize, viewport_height: u16) {
        let max_scroll = tips_scroll_max(self.sidebar_view(), viewport_height);
        let current = self.tips_scroll.min(max_scroll);
        self.tips_scroll = offset_after_scroll(current, delta, max_scroll);
    }

    fn target_index_at(&self, column: u16, row: u16, area: Rect) -> Option<usize> {
        let areas = self.sidebar_areas(area);
        let scroll = self.tree_scroll.min(tree_scroll_max_for_len(
            self.targets().len(),
            areas.tree.height.saturating_sub(2),
        ));
        target_index_at_tree_area(self.targets().len(), areas.tree, column, row, scroll)
    }

    #[cfg(test)]
    fn comms_index_at(&self, column: u16, row: u16, area: Rect) -> Option<usize> {
        self.comms_action_at(column, row, area)
            .map(|(index, _)| index)
    }

    fn comms_action_at(
        &self,
        column: u16,
        row: u16,
        area: Rect,
    ) -> Option<(usize, CommsMouseAction)> {
        let areas = self.sidebar_areas(area);
        let prefix_lines = self.comms_prefix_lines();
        let items = self.visible_comms_limited();
        let content_width = usize::from(areas.comms.width.saturating_sub(2));
        let scroll = self.comms_scroll.min(comms_scroll_max_for_items(
            &items,
            comms_body_capacity(areas.comms, prefix_lines),
            content_width,
            self.sidebar_view().comms_compact,
            self.theme(),
        ));
        let visible_items = items.into_iter().skip(scroll).collect::<Vec<_>>();
        comms_action_at_area(
            &visible_items,
            areas.comms,
            (column, row),
            content_width,
            self.sidebar_view().comms_compact,
            prefix_lines,
            self.theme(),
        )
        .map(|(index, action)| (index + scroll, action))
    }

    fn comms_scroll_max(&self, area: Rect) -> usize {
        let prefix_lines = self.comms_prefix_lines();
        let items = self.visible_comms_limited();
        comms_scroll_max_for_items(
            &items,
            comms_body_capacity(area, prefix_lines),
            usize::from(area.width.saturating_sub(2)),
            self.sidebar_view().comms_compact,
            self.theme(),
        )
    }

    fn visible_comms(&self) -> Vec<CommsItem> {
        self.view()
            .map(|view| {
                view.comms
                    .iter()
                    .filter(|item| !self.hidden_comms.contains(&item.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_comms_limited(&self) -> Vec<CommsItem> {
        let limit = self.sidebar_view().comms_limit.max(1);
        self.visible_comms().into_iter().take(limit).collect()
    }

    fn sidebar_view(&self) -> &SidebarViewInfo {
        if let Some(view) = self.view() {
            &view.namespace.sidebar.view
        } else {
            default_sidebar_view()
        }
    }

    fn sidebar_areas(&self, area: Rect) -> SidebarAreas {
        sidebar_areas_with_override(area, self.sidebar_view(), self.section_heights)
    }

    fn start_resize_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        let Some(divider) = self.resize_divider_at(column, row, area) else {
            return false;
        };
        self.dragging_divider = Some(divider);
        true
    }

    fn resize_divider_at(&self, column: u16, row: u16, area: Rect) -> Option<SidebarResizeDivider> {
        if column < area.x || column >= area.x.saturating_add(area.width) {
            return None;
        }
        let areas = self.sidebar_areas(area);
        if areas.comms.height > 0 && row == areas.comms.y {
            return Some(SidebarResizeDivider::TreeComms);
        }
        if let Some(tips) = areas.tips
            && tips.height > 0
            && row == tips.y
        {
            return Some(SidebarResizeDivider::CommsTips);
        }
        None
    }

    fn drag_resize_to(
        &mut self,
        divider: SidebarResizeDivider,
        _column: u16,
        row: u16,
        area: Rect,
    ) -> bool {
        if area.height == 0 {
            return false;
        }
        let (tree_height, _, tips_height) = self.current_section_heights(area);
        match divider {
            SidebarResizeDivider::TreeComms => {
                if self.sidebar_view().tips_enabled {
                    let available = area.height.saturating_sub(tips_height);
                    if available < MIN_SECTION_HEIGHT.saturating_mul(2) {
                        return false;
                    }
                    let desired_tree = row.saturating_sub(area.y);
                    let new_tree = desired_tree.clamp(
                        MIN_SECTION_HEIGHT,
                        available.saturating_sub(MIN_SECTION_HEIGHT),
                    );
                    self.section_heights =
                        Some((new_tree, available.saturating_sub(new_tree), tips_height));
                } else {
                    if area.height < MIN_SECTION_HEIGHT.saturating_mul(2) {
                        return false;
                    }
                    let desired_tree = row.saturating_sub(area.y);
                    let new_tree = desired_tree.clamp(
                        MIN_SECTION_HEIGHT,
                        area.height.saturating_sub(MIN_SECTION_HEIGHT),
                    );
                    self.section_heights =
                        Some((new_tree, area.height.saturating_sub(new_tree), 0));
                }
            }
            SidebarResizeDivider::CommsTips => {
                if !self.sidebar_view().tips_enabled {
                    return false;
                }
                let available = area.height.saturating_sub(tree_height);
                if available < MIN_SECTION_HEIGHT.saturating_mul(2) {
                    return false;
                }
                let desired_comms = row.saturating_sub(area.y).saturating_sub(tree_height);
                let new_comms = desired_comms.clamp(
                    MIN_SECTION_HEIGHT,
                    available.saturating_sub(MIN_SECTION_HEIGHT),
                );
                self.section_heights =
                    Some((tree_height, new_comms, available.saturating_sub(new_comms)));
            }
        }
        true
    }

    fn current_section_heights(&self, area: Rect) -> (u16, u16, u16) {
        let areas = self.sidebar_areas(area);
        (
            areas.tree.height,
            areas.comms.height,
            areas.tips.map(|area| area.height).unwrap_or(0),
        )
    }

    fn sidebar_config_error(&self) -> Option<&str> {
        self.view()
            .and_then(|view| view.namespace.sidebar.view_error.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn tree_degraded_label(&self) -> Option<&'static str> {
        if self.last_error.is_some() {
            Some("ccbd ✕")
        } else if self.sidebar_config_error().is_some() {
            Some("config ✕")
        } else if matches!(
            self.config_ui_launch_status(),
            Some(ConfigUiLaunchStatus::Failed(_))
        ) {
            Some("config ui ✕")
        } else if matches!(
            self.config_ui_launch_status(),
            Some(ConfigUiLaunchStatus::Manual(_))
        ) {
            Some("config ui !")
        } else {
            None
        }
    }

    fn config_ui_launch_status(&self) -> Option<ConfigUiLaunchStatus> {
        let launch = self.config_ui_launch.as_ref()?;
        match launch.lock() {
            Ok(status) => Some(status.clone()),
            Err(_) => Some(ConfigUiLaunchStatus::Failed(
                "status unavailable; click to retry".to_string(),
            )),
        }
    }

    fn config_ui_launch_is_active(&self) -> bool {
        matches!(
            self.config_ui_launch_status(),
            Some(
                ConfigUiLaunchStatus::Opening
                    | ConfigUiLaunchStatus::Ready(_)
                    | ConfigUiLaunchStatus::Manual(_)
            )
        )
    }

    fn config_ui_status_line(&self) -> Option<String> {
        match self.config_ui_launch_status()? {
            ConfigUiLaunchStatus::Opening => Some("config ui: opening...".to_string()),
            ConfigUiLaunchStatus::Ready(url) => Some(format!("config ui: {url}")),
            ConfigUiLaunchStatus::Manual(url) => Some(format!("config ui open manually: {url}")),
            ConfigUiLaunchStatus::Failed(error) => Some(format!("config ui failed: {error}")),
        }
    }

    fn comms_prefix_lines(&self) -> u16 {
        u16::from(self.last_error.is_some())
            .saturating_add(u16::from(self.sidebar_config_error().is_some()))
            .saturating_add(u16::from(self.config_ui_status_line().is_some()))
    }

    fn view(&self) -> Option<&ProjectView> {
        self.response.as_ref().map(|response| &response.view)
    }

    fn targets(&self) -> Vec<RowTarget> {
        self.view().map(row_targets).unwrap_or_default()
    }

    fn refresh_interval(&self) -> Duration {
        let ttl_ms = self
            .response
            .as_ref()
            .map(|response| response.cache.ttl_ms)
            .filter(|ttl_ms| *ttl_ms > 0)
            .unwrap_or(PROJECT_VIEW_REFRESH_DEFAULT_MS);
        Duration::from_millis(
            ttl_ms.clamp(PROJECT_VIEW_REFRESH_MIN_MS, PROJECT_VIEW_REFRESH_MAX_MS),
        )
    }

    fn clamp_selection(&mut self) {
        let count = self.targets().len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn select_active_target(&mut self) {
        if let Some(index) = self.active_target_index() {
            self.selected = index;
        } else {
            self.clamp_selection();
        }
    }

    fn active_target_index(&self) -> Option<usize> {
        let view = self.view()?;
        let targets = row_targets(view);

        if let Some(active_agent) = view.agents.iter().find(|agent| agent.active) {
            let target = RowTarget::Agent(active_agent.name.clone());
            if let Some(index) = targets.iter().position(|candidate| candidate == &target) {
                return Some(index);
            }
        }

        if let Some(active_window) = view.windows.iter().find(|window| window.active) {
            let target = RowTarget::Window(active_window.name.clone());
            if let Some(index) = targets.iter().position(|candidate| candidate == &target) {
                return Some(index);
            }
        }

        let active_window = view
            .namespace
            .active_window
            .as_deref()
            .map(str::trim)
            .filter(|window| !window.is_empty())?;
        let target = RowTarget::Window(active_window.to_string());
        targets.iter().position(|candidate| candidate == &target)
    }

    fn focus_target(&mut self, client: &CcbdClient, target: RowTarget) {
        match request_focus(client, &target, self.namespace_epoch()) {
            Ok(()) => self.force_refresh(),
            Err(err) if is_stale_view_error(&err) => {
                self.retry_focus_after_stale_view(client, target)
            }
            Err(err) => self.set_error(err),
        }
    }

    fn retry_focus_after_stale_view(&mut self, client: &CcbdClient, target: RowTarget) {
        match client.project_view() {
            Ok(response) => self.apply_response(response),
            Err(err) => {
                self.set_error(err);
                return;
            }
        }
        match request_focus(client, &target, self.namespace_epoch()) {
            Ok(()) => self.force_refresh(),
            Err(err) => self.set_error(err),
        }
    }

    fn recover_comms_item(&mut self, client: &CcbdClient, item: &CommsItem) {
        let job_id = recover_job_id(item).unwrap_or(item.id.as_str());
        let reply_delivery_job_id =
            recover_reply_delivery_job_id(item).or(item.reply_delivery_job_id.as_deref());
        match client.comms_recover(job_id, reply_delivery_job_id, item.block_reason.as_deref()) {
            Ok(()) => self.force_refresh(),
            Err(err) => self.set_error(err),
        }
    }

    fn cancel_comms_item(&mut self, client: &CcbdClient, item: &CommsItem) {
        let job_id = recover_job_id(item).unwrap_or(item.id.as_str());
        match client.cancel(job_id) {
            Ok(()) => self.force_refresh(),
            Err(err) if is_terminal_cancel_error(&err) => {
                self.hide_comms_item(item);
                self.force_refresh();
            }
            Err(err) => self.set_error(err),
        }
    }

    fn dismiss_comms_item(&mut self, client: &CcbdClient, item: &CommsItem) {
        match client.dismiss_comms(&item.id) {
            Ok(()) => {
                self.hide_comms_item(item);
                self.force_refresh();
            }
            Err(err) => self.set_error(err),
        }
    }

    fn hide_comms_item(&mut self, item: &CommsItem) {
        if !item.id.trim().is_empty() {
            self.hidden_comms.insert(item.id.clone());
        }
    }
}

fn request_focus(
    client: &CcbdClient,
    target: &RowTarget,
    namespace_epoch: Option<u64>,
) -> Result<(), String> {
    match target {
        RowTarget::Window(window) => client.focus_window(window, namespace_epoch),
        RowTarget::Agent(agent) => client.focus_agent(agent, namespace_epoch),
    }
}

fn is_stale_view_error(error: &str) -> bool {
    error.trim() == "stale_view" || error.contains("stale_view")
}

fn refresh_backoff_for_failures(failure_count: u32) -> Duration {
    if failure_count <= 1 {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(5)
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &SidebarApp) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let areas = app.sidebar_areas(area);
    draw_tree(frame, areas.tree, app);
    draw_comms(frame, areas.comms, app);
    if let Some(tips_area) = areas.tips {
        draw_tips(frame, tips_area, app);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarAreas {
    tree: Rect,
    comms: Rect,
    tips: Option<Rect>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn sidebar_areas(area: Rect, view: &SidebarViewInfo) -> SidebarAreas {
    sidebar_areas_with_override(area, view, None)
}

fn sidebar_areas_with_override(
    area: Rect,
    view: &SidebarViewInfo,
    section_heights: Option<(u16, u16, u16)>,
) -> SidebarAreas {
    if area.height == 0 {
        return SidebarAreas {
            tree: area,
            comms: Rect::new(area.x, area.y, area.width, 0),
            tips: None,
        };
    }
    if let Some(heights) = section_heights {
        if view.tips_enabled {
            if let Some((tree_height, comms_height, tips_height)) =
                clamp_three_section_heights(area.height, heights)
            {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(tree_height),
                        Constraint::Length(comms_height),
                        Constraint::Length(tips_height),
                    ])
                    .split(area);
                return SidebarAreas {
                    tree: chunks[0],
                    comms: chunks[1],
                    tips: (tips_height > 0).then_some(chunks[2]),
                };
            }
        } else if let Some((tree_height, comms_height)) =
            clamp_two_section_heights(area.height, (heights.0, heights.1))
        {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(tree_height),
                    Constraint::Length(comms_height),
                ])
                .split(area);
            return SidebarAreas {
                tree: chunks[0],
                comms: chunks[1],
                tips: None,
            };
        }
    }
    if !view.tips_enabled {
        let tree_height = tree_height_for(area.height, view).min(area.height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(tree_height), Constraint::Min(0)])
            .split(area);
        return SidebarAreas {
            tree: chunks[0],
            comms: chunks[1],
            tips: None,
        };
    }
    let tree_height = tree_height_for(area.height, view).min(area.height);
    let remaining_after_tree = area.height.saturating_sub(tree_height);
    let desired_comms_height = comms_height_for(area.height, view);
    let comms_height = desired_comms_height.min(remaining_after_tree);
    let remaining_after_comms = remaining_after_tree.saturating_sub(comms_height);
    let desired_tips_height = tips_height_for(area.height, view);
    let tips_height = desired_tips_height.min(remaining_after_comms);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tree_height),
            Constraint::Length(comms_height),
            Constraint::Length(tips_height),
        ])
        .split(area);
    SidebarAreas {
        tree: chunks[0],
        comms: chunks[1],
        tips: (tips_height > 0).then_some(chunks[2]),
    }
}

fn clamp_three_section_heights(
    total_height: u16,
    requested: (u16, u16, u16),
) -> Option<(u16, u16, u16)> {
    if total_height < MIN_SECTION_HEIGHT.saturating_mul(3) {
        return None;
    }
    let tree_height = requested.0.clamp(
        MIN_SECTION_HEIGHT,
        total_height.saturating_sub(MIN_SECTION_HEIGHT.saturating_mul(2)),
    );
    let remaining_after_tree = total_height.saturating_sub(tree_height);
    let comms_height = requested.1.clamp(
        MIN_SECTION_HEIGHT,
        remaining_after_tree.saturating_sub(MIN_SECTION_HEIGHT),
    );
    let tips_height = total_height
        .saturating_sub(tree_height)
        .saturating_sub(comms_height);
    Some((tree_height, comms_height, tips_height))
}

fn clamp_two_section_heights(total_height: u16, requested: (u16, u16)) -> Option<(u16, u16)> {
    if total_height < MIN_SECTION_HEIGHT.saturating_mul(2) {
        return None;
    }
    let tree_height = requested.0.clamp(
        MIN_SECTION_HEIGHT,
        total_height.saturating_sub(MIN_SECTION_HEIGHT),
    );
    let comms_height = total_height.saturating_sub(tree_height);
    Some((tree_height, comms_height))
}

fn tree_height_for(total_height: u16, view: &SidebarViewInfo) -> u16 {
    view_height_for(
        total_height,
        &view.agents_height,
        DEFAULT_TREE_HEIGHT_PERCENT,
    )
}

fn comms_height_for(total_height: u16, view: &SidebarViewInfo) -> u16 {
    view_height_for(
        total_height,
        &view.comms_height,
        DEFAULT_COMMS_HEIGHT_PERCENT,
    )
}

fn tips_height_for(total_height: u16, view: &SidebarViewInfo) -> u16 {
    view_height_for(total_height, &view.tips_height, DEFAULT_TIPS_HEIGHT_PERCENT)
}

fn view_height_for(total_height: u16, value: &serde_json::Value, default_percent: u16) -> u16 {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or_else(|| percent_height(total_height, default_percent)),
        serde_json::Value::String(text) => parse_height_value(total_height, text)
            .unwrap_or_else(|| percent_height(total_height, default_percent)),
        _ => percent_height(total_height, default_percent),
    }
    .clamp(3.min(total_height), total_height)
}

fn parse_height_value(total_height: u16, value: &str) -> Option<u16> {
    let text = value.trim();
    if let Some(percent_text) = text.strip_suffix('%') {
        let percent = percent_text.trim().parse::<u16>().ok()?;
        if percent == 0 || percent >= 100 {
            return None;
        }
        return Some(percent_height(total_height, percent));
    }
    let fixed = text.parse::<u16>().ok()?;
    (fixed > 0).then_some(fixed)
}

fn percent_height(total_height: u16, percent: u16) -> u16 {
    let numerator = u32::from(total_height) * u32::from(percent);
    let value = numerator.saturating_add(99) / 100;
    u16::try_from(value).unwrap_or(total_height).max(1)
}

fn default_sidebar_view() -> &'static SidebarViewInfo {
    static DEFAULT: std::sync::OnceLock<SidebarViewInfo> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(SidebarViewInfo::default)
}

fn target_index_at_tree_area(
    target_count: usize,
    area: Rect,
    column: u16,
    row: u16,
    scroll_offset: usize,
) -> Option<usize> {
    if target_count == 0 || area.width < 3 || area.height < 3 {
        return None;
    }
    let left = area.x.saturating_add(1);
    let right = area.x.saturating_add(area.width.saturating_sub(1));
    if column < left || column >= right {
        return None;
    }
    let top = area.y.saturating_add(1);
    let bottom = area.y.saturating_add(area.height.saturating_sub(1));
    if row < top || row >= bottom {
        return None;
    }
    let index = usize::from(row - top).saturating_add(scroll_offset);
    if index < target_count {
        Some(index)
    } else {
        None
    }
}

fn offset_after_scroll(current: usize, delta: isize, max_scroll: usize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max_scroll)
    }
}

fn tree_scroll_max_for_len(row_count: usize, viewport_height: u16) -> usize {
    row_count.saturating_sub(usize::from(viewport_height.max(1)))
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommsMouseAction {
    Select,
    Retry,
    Cancel,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderMouseAction {
    Settings,
    KillProject,
}

fn header_action_at(column: u16, row: u16, area: Rect) -> Option<HeaderMouseAction> {
    let controls = tree_controls_area(area);
    if controls.width == 0 || row != controls.y {
        return None;
    }
    if column < controls.x || column >= controls.x.saturating_add(controls.width) {
        return None;
    }
    let relative_column = column.saturating_sub(controls.x);
    if relative_column <= 1 {
        Some(HeaderMouseAction::Settings)
    } else if relative_column == 2 {
        Some(HeaderMouseAction::KillProject)
    } else {
        None
    }
}

fn comms_action_at_area(
    items: &[CommsItem],
    area: Rect,
    position: (u16, u16),
    width: usize,
    compact: bool,
    prefix_lines: u16,
    theme: SidebarTheme,
) -> Option<(usize, CommsMouseAction)> {
    let (column, row) = position;
    if items.is_empty() || area.width < 3 || area.height < 3 {
        return None;
    }
    let left = area.x.saturating_add(1);
    let right = area.x.saturating_add(area.width.saturating_sub(1));
    if column < left || column >= right {
        return None;
    }
    let top = area.y.saturating_add(1);
    let bottom = area.y.saturating_add(area.height.saturating_sub(1));
    if row < top || row >= bottom {
        return None;
    }
    let mut current = top.saturating_add(prefix_lines);
    for (index, item) in items.iter().enumerate() {
        let height = comms_lines_with_theme(item, width, compact, theme)
            .len()
            .max(1) as u16;
        if row >= current && row < current.saturating_add(height) {
            if row != current {
                return Some((index, CommsMouseAction::Select));
            }
            let relative_column = column.saturating_sub(left);
            return Some((index, comms_mouse_action_for_column(relative_column)));
        }
        current = current.saturating_add(height);
        if current >= bottom {
            break;
        }
    }
    None
}

fn comms_mouse_action_for_column(column: u16) -> CommsMouseAction {
    if COMMS_ACTION_RETRY_COLS.contains(&column) {
        CommsMouseAction::Retry
    } else if COMMS_ACTION_CANCEL_COLS.contains(&column) {
        CommsMouseAction::Cancel
    } else if COMMS_ACTION_CLEAR_COLS.contains(&column) {
        CommsMouseAction::Clear
    } else {
        CommsMouseAction::Select
    }
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, app: &SidebarApp) {
    let title = app
        .view()
        .map(|view| tree_title(view, app, tree_title_width(area.width)))
        .unwrap_or_else(|| {
            tree_title_from_parts(
                &app.pane_window,
                None,
                app.tree_degraded_label(),
                tree_title_width(area.width),
            )
        });
    let focus_style = tree_focus_style(app);
    let rows = tree_rows(app);
    let row_count = rows.len();
    let content_height = area.height.saturating_sub(2);
    let needs_scrollbar = content_height > 0 && row_count > usize::from(content_height);
    let scroll = app
        .tree_scroll
        .min(tree_scroll_max_for_len(row_count, content_height));
    let items = rows
        .into_iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(content_height.max(1)))
        .map(|(index, item)| {
            if index == app.selected {
                item.style(app.theme().selection_style())
            } else {
                item
            }
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(
        Block::default()
            .title_top(Line::from(title).style(focus_style).left_aligned())
            .title_top(tree_controls_line(app.theme()).right_aligned())
            .borders(Borders::ALL)
            .border_style(focus_style),
    );
    frame.render_widget(list, area);
    if needs_scrollbar {
        let scrollbar = sidebar_scrollbar(app.theme());
        let scrollbar_position =
            scrollbar_position_for_scroll(scroll, row_count, usize::from(content_height));
        let mut scrollbar_state = ScrollbarState::new(row_count)
            .position(scrollbar_position)
            .viewport_content_length(usize::from(content_height));
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn tree_rows(app: &SidebarApp) -> Vec<ListItem<'static>> {
    let mut rows = Vec::new();
    if let Some(view) = app.view() {
        for window in &view.windows {
            if !window.show_in_sidebar {
                continue;
            }
            rows.push(window_row(window));
            for agent in view
                .agents
                .iter()
                .filter(|agent| agent.window == window.name)
            {
                rows.push(agent_row(agent, app.theme()));
            }
        }
    }
    if rows.is_empty() {
        rows.push(ListItem::new(Line::from(if app.last_error.is_some() {
            "ccbd unavailable"
        } else {
            "waiting for ProjectView"
        })));
    }
    rows
}

fn tree_controls_area(area: Rect) -> Rect {
    if area.width < TREE_CONTROL_CONTENT_WIDTH + 2 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect::new(
        area.x + area.width - TREE_CONTROL_CONTENT_WIDTH - 1,
        area.y,
        TREE_CONTROL_CONTENT_WIDTH,
        1,
    )
}

fn tree_title_width(width: u16) -> u16 {
    width.saturating_sub(TREE_CONTROL_CONTENT_WIDTH + 1)
}

fn tree_controls_line(theme: SidebarTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            TREE_SETTINGS_SYMBOL,
            Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            TREE_KILL_SYMBOL,
            Style::default()
                .fg(theme.danger)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn tree_title(view: &ProjectView, app: &SidebarApp, width: u16) -> String {
    tree_title_from_parts(
        &app.pane_window,
        view.namespace.active_window.as_deref(),
        app.tree_degraded_label(),
        width,
    )
}

fn tree_title_from_parts(
    pane_window: &str,
    active_window: Option<&str>,
    degraded_label: Option<&str>,
    width: u16,
) -> String {
    let active = active_window
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(pane_window);
    let cross_window_focus = active != pane_window;
    let title = if cross_window_focus {
        degraded_label
            .map(|label| format!("focus:{active} · {label}"))
            .unwrap_or_else(|| format!("focus:{active}"))
    } else if let Some(label) = degraded_label {
        label.to_string()
    } else {
        String::new()
    };
    let available = usize::from(width.saturating_sub(2));
    if title.chars().count() <= available {
        title
    } else if cross_window_focus && degraded_label.is_some() {
        "focus warn".to_string()
    } else if cross_window_focus {
        "focus".to_string()
    } else if degraded_label.is_some() {
        "warn".to_string()
    } else {
        String::new()
    }
}

fn tree_focus_style(app: &SidebarApp) -> Style {
    Style::default().fg(app.theme().focus)
}

fn window_row(window: &WindowView) -> ListItem<'static> {
    let active = if window.active { ">" } else { " " };
    let label = if window.label.is_empty() {
        window.name.clone()
    } else {
        window.label.clone()
    };
    ListItem::new(Line::from(vec![
        Span::raw(format!("{active} ")),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
    ]))
}

fn agent_row(agent: &AgentView, theme: SidebarTheme) -> ListItem<'static> {
    ListItem::new(Line::from(agent_row_spans(agent, theme)))
}

pub(crate) fn agent_row_spans(agent: &AgentView, theme: SidebarTheme) -> Vec<Span<'static>> {
    let drain_label = reload_drain_label(agent);
    let draining = drain_label.is_some();
    let auth_blocked = !draining && agent.reason.as_deref() == Some("provider_auth_revoked");
    let state = if draining {
        "pending"
    } else if agent.activity_state.is_empty() {
        "offline"
    } else {
        agent.activity_state.as_str()
    };
    let symbol = if draining {
        "◐"
    } else {
        agent
            .activity_symbol
            .as_deref()
            .unwrap_or_else(|| activity_symbol(state))
    };
    let explicit_color = if draining {
        Some("yellow")
    } else {
        agent.activity_color.as_deref()
    };
    let active = if agent.active { "*" } else { " " };
    let display_name = if agent.display_name.trim().is_empty() {
        agent.name.clone()
    } else {
        agent.display_name.clone()
    };
    let provider_display_name = if agent.provider_display_name.trim().is_empty() {
        agent.provider.clone()
    } else {
        agent.provider_display_name.clone()
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            symbol.to_string(),
            Style::default().fg(activity_color_with_theme(state, explicit_color, theme)),
        ),
        Span::raw(format!("{active} ")),
        Span::raw(display_name),
    ];
    if auth_blocked {
        spans.push(Span::styled(" [login]", Style::default().fg(theme.danger)));
    } else {
        spans.push(Span::raw(format!(" [{}]", provider_display_name)));
    }
    if agent.queue_depth > 0 {
        spans.push(Span::styled(
            format!(" ·{}", agent.queue_depth),
            Style::default().fg(theme.muted),
        ));
    }
    if let Some(job_id) = agent
        .current_job_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let suffix = if job_id.len() >= 4 {
            &job_id[job_id.len() - 4..]
        } else {
            job_id
        };
        spans.push(Span::styled(
            format!(" #{suffix}"),
            Style::default().fg(theme.muted),
        ));
    }
    if let Some(label) = drain_label {
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme.warning),
        ));
    }
    spans
}

fn reload_drain_label(agent: &AgentView) -> Option<String> {
    if !agent.dispatch_blocked_by_reload_drain && agent.reload_drain.is_none() {
        return None;
    }
    let status = agent
        .reload_drain
        .as_ref()
        .map(|record| record.status.trim())
        .unwrap_or_default();
    if status.is_empty() {
        Some("drain".to_string())
    } else {
        Some(format!("drain:{status}"))
    }
}

fn draw_comms(frame: &mut Frame<'_>, area: Rect, app: &SidebarApp) {
    let mut lines = Vec::new();
    if app.last_error.is_some() {
        lines.push(Line::from(Span::styled(
            if app.view().is_some() {
                "stale ProjectView"
            } else {
                "ccbd unavailable"
            },
            Style::default().fg(app.theme().warning),
        )));
    }
    if let Some(error) = app.sidebar_config_error() {
        lines.push(Line::from(Span::styled(
            truncate_comms_preview(
                &format!("config error: {error}"),
                usize::from(area.width.saturating_sub(2)),
            ),
            Style::default().fg(app.theme().warning),
        )));
    }
    if let Some(status) = app.config_ui_status_line() {
        lines.push(Line::from(Span::styled(
            truncate_comms_preview(&status, usize::from(area.width.saturating_sub(2))),
            Style::default().fg(
                if status.starts_with("config ui failed:")
                    || status.starts_with("config ui open manually:")
                {
                    app.theme().warning
                } else {
                    app.theme().info
                },
            ),
        )));
    }
    let prefix_lines = lines.len() as u16;
    if app.view().is_some() {
        let visible_comms = app.visible_comms_limited();
        let compact = app.sidebar_view().comms_compact;
        let body_capacity = comms_body_capacity(area, prefix_lines);
        let full_width = usize::from(area.width.saturating_sub(2));
        let visible_capacity = comms_visible_item_capacity(
            &visible_comms,
            body_capacity,
            full_width,
            compact,
            app.theme(),
        );
        let needs_scrollbar = body_capacity > 0 && visible_comms.len() > visible_capacity;
        let content_width =
            usize::from(
                area.width
                    .saturating_sub(if needs_scrollbar { 3 } else { 2 }),
            );
        let scroll = app.comms_scroll.min(comms_scroll_max_for_items(
            &visible_comms,
            body_capacity,
            content_width,
            compact,
            app.theme(),
        ));
        let mut body_lines_used = 0usize;
        for item in visible_comms.iter().skip(scroll) {
            let item_lines = comms_lines_with_theme(item, content_width, compact, app.theme());
            if body_lines_used + item_lines.len() > body_capacity {
                if body_lines_used == 0 && body_capacity > 0 {
                    lines.extend(item_lines.into_iter().take(body_capacity));
                }
                break;
            }
            body_lines_used += item_lines.len();
            lines.extend(item_lines);
        }
        if needs_scrollbar {
            let scrollbar = sidebar_scrollbar(app.theme());
            let scrollbar_position =
                scrollbar_position_for_scroll(scroll, visible_comms.len(), visible_capacity);
            let mut scrollbar_state = ScrollbarState::new(visible_comms.len())
                .position(scrollbar_position)
                .viewport_content_length(visible_capacity);
            let paragraph =
                Paragraph::new(lines).block(Block::default().title("Comms").borders(Borders::ALL));
            frame.render_widget(paragraph, area);
            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
            return;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("no comms"));
    }
    let paragraph =
        Paragraph::new(lines).block(Block::default().title("Comms").borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn comms_body_capacity(area: Rect, prefix_lines: u16) -> usize {
    usize::from(area.height.saturating_sub(2))
        .saturating_sub(usize::from(prefix_lines))
        .max(1)
}

fn comms_visible_item_capacity(
    items: &[CommsItem],
    body_capacity: usize,
    width: usize,
    compact: bool,
    theme: SidebarTheme,
) -> usize {
    if items.is_empty() {
        return 0;
    }
    let mut used = 0usize;
    let mut count = 0usize;
    for item in items {
        let height = comms_lines_with_theme(item, width, compact, theme)
            .len()
            .max(1);
        if count > 0 && used.saturating_add(height) > body_capacity {
            break;
        }
        if count == 0 && height > body_capacity {
            return 1;
        }
        used = used.saturating_add(height);
        count += 1;
    }
    count.max(1).min(items.len())
}

fn comms_scroll_max_for_items(
    items: &[CommsItem],
    body_capacity: usize,
    width: usize,
    compact: bool,
    theme: SidebarTheme,
) -> usize {
    items.len().saturating_sub(comms_visible_item_capacity(
        items,
        body_capacity,
        width,
        compact,
        theme,
    ))
}

fn draw_tips(frame: &mut Frame<'_>, area: Rect, app: &SidebarApp) {
    if area.height == 0 {
        return;
    }
    let content_height = usize::from(area.height.saturating_sub(2));
    let tips_count = app.sidebar_view().tips.len();
    let needs_scrollbar = content_height > 0 && tips_count > content_height;
    let content_width = usize::from(
        area.width
            .saturating_sub(if needs_scrollbar { 3 } else { 2 }),
    );
    let mut lines = app
        .sidebar_view()
        .tips
        .iter()
        .map(|tip| Line::from(truncate_comms_preview(tip, content_width)))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from("no tips"));
    }
    let scroll = app
        .tips_scroll
        .min(lines.len().saturating_sub(content_height.max(1)));
    let visible_lines = lines
        .into_iter()
        .skip(scroll)
        .take(content_height.max(1))
        .collect::<Vec<_>>();
    let paragraph =
        Paragraph::new(visible_lines).block(Block::default().title("Tips").borders(Borders::ALL));
    frame.render_widget(paragraph, area);
    if needs_scrollbar {
        let scrollbar = sidebar_scrollbar(app.theme());
        let scrollbar_position = scrollbar_position_for_scroll(scroll, tips_count, content_height);
        let mut scrollbar_state = ScrollbarState::new(tips_count)
            .position(scrollbar_position)
            .viewport_content_length(content_height);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn tips_scroll_max(view: &SidebarViewInfo, viewport_height: u16) -> usize {
    view.tips
        .len()
        .saturating_sub(usize::from(viewport_height.max(1)))
}

fn sidebar_scrollbar(theme: SidebarTheme) -> Scrollbar<'static> {
    let track_style = Style::default().fg(theme.scrollbar_track);
    let thumb_style = Style::default().fg(theme.scrollbar_thumb);
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .begin_style(track_style)
        .end_style(track_style)
        .track_style(track_style)
        .thumb_style(thumb_style)
}

fn scrollbar_position_for_scroll(
    scroll: usize,
    content_length: usize,
    viewport_content_length: usize,
) -> usize {
    if content_length == 0 || viewport_content_length == 0 {
        return 0;
    }
    let max_scroll = content_length.saturating_sub(viewport_content_length);
    if max_scroll == 0 {
        return 0;
    }
    let max_position = content_length.saturating_sub(1);
    scroll.min(max_scroll).saturating_mul(max_position) / max_scroll
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

#[cfg(test)]
fn comms_line_text(item: &CommsItem) -> String {
    comms_lines(item, 80, true)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
fn comms_lines(item: &CommsItem, width: usize, compact: bool) -> Vec<Line<'static>> {
    comms_lines_with_theme(item, width, compact, SidebarTheme::default_dark())
}

fn comms_lines_with_theme(
    item: &CommsItem,
    width: usize,
    compact: bool,
    theme: SidebarTheme,
) -> Vec<Line<'static>> {
    if compact {
        return compact_comms_lines(item, width, theme);
    }
    let status = comms_display_status(item);
    let preview = item.body_preview.trim();
    let reason = comms_reason(item)
        .map(|value| format!(" {value}"))
        .unwrap_or_default();
    let mut first_line_spans = comms_action_spans(item, theme);
    first_line_spans.push(Span::raw(format!(
        "{} > {}",
        empty_dash(&item.sender),
        empty_dash(&item.target)
    )));
    push_comms_short_id_span(&mut first_line_spans, item, theme);
    first_line_spans.push(Span::styled(
        compact_comms_status(status).to_string(),
        Style::default().fg(comms_status_color_with_theme(item, theme)),
    ));
    let mut lines = vec![Line::from(first_line_spans)];
    if !preview.is_empty() {
        lines.push(Line::from(truncate_comms_preview(preview, width)));
    }
    if !reason.is_empty() {
        lines.push(Line::from(truncate_comms_preview(reason.trim(), width)));
    }
    lines
}

fn compact_comms_lines(item: &CommsItem, width: usize, theme: SidebarTheme) -> Vec<Line<'static>> {
    let status = comms_display_status(item);
    let route = format!(
        "{} > {}",
        empty_dash(&item.sender),
        empty_dash(&item.target)
    );
    let compact_status = compact_comms_status(status).to_string();
    let mut spans = comms_action_spans(item, theme);
    spans.push(Span::raw(route.clone()));
    push_comms_short_id_span(&mut spans, item, theme);
    spans.push(Span::styled(
        compact_status.clone(),
        Style::default().fg(comms_status_color_with_theme(item, theme)),
    ));
    vec![Line::from(spans), compact_comms_detail_line(item, width)]
}

fn push_comms_short_id_span(spans: &mut Vec<Span<'static>>, item: &CommsItem, theme: SidebarTheme) {
    let token = comms_short_id_token(&item.short_id, &item.id);
    if token.is_empty() {
        return;
    }
    spans.push(Span::styled(token, Style::default().fg(theme.muted)));
}

fn comms_short_id_token(short_id: &str, full_id: &str) -> String {
    let trimmed = short_id.trim();
    if !trimmed.is_empty() {
        return format!(" #{trimmed} ");
    }
    let fallback = full_id.trim();
    if fallback.len() >= 4 {
        let suffix = &fallback[fallback.len() - 4..];
        return format!(" #{suffix} ");
    }
    if !fallback.is_empty() {
        return format!(" #{fallback} ");
    }
    " ".to_string()
}

fn compact_comms_detail_line(item: &CommsItem, width: usize) -> Line<'static> {
    let indent = " ".repeat(COMMS_DETAIL_INDENT_WIDTH.min(width));
    let available = width.saturating_sub(COMMS_DETAIL_INDENT_WIDTH);
    let detail = compact_comms_detail(item);
    if detail.is_empty() || available == 0 {
        return Line::from(indent);
    }
    Line::from(format!(
        "{indent}{}",
        truncate_comms_preview(&detail, available)
    ))
}

fn compact_comms_detail(item: &CommsItem) -> String {
    let preview = item.body_preview.trim();
    let reason = comms_reason(item).unwrap_or_default();
    let reason = reason.trim();
    match (preview.is_empty(), reason.is_empty()) {
        (true, true) => String::new(),
        (false, true) => preview.to_string(),
        (true, false) => reason.to_string(),
        (false, false) => format!("{preview} {reason}"),
    }
}

fn comms_action_spans(_item: &CommsItem, theme: SidebarTheme) -> Vec<Span<'static>> {
    let retry_style = Style::default()
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let cancel_style = Style::default()
        .fg(theme.danger)
        .add_modifier(Modifier::BOLD);
    vec![
        Span::styled("↻ ", retry_style),
        Span::raw(" "),
        Span::styled("X ", cancel_style),
        Span::raw(" "),
        Span::styled(
            "⌫ ",
            Style::default()
                .fg(theme.clear)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]
}

fn compact_comms_status(value: &str) -> &str {
    match value.trim() {
        "queued" => "que",
        "injecting" => "inj",
        "executing" => "run",
        "provider_idle_pending_terminal" => "idle",
        "reply_queued" => "r-q",
        "reply_delivering" => "back",
        "orphaned" => "orph",
        "terminal" => "ok",
        "unknown" => "?",
        "send" | "sending" => "snd",
        "back" | "replying" => "rep",
        "work" | "running" => "run",
        "done" | "completed" | "replied" => "ok",
        "fail" | "failed" | "delivery_failed" => "err",
        "cancelled" | "canceled" => "cnl",
        other => other,
    }
}

fn comms_display_status(item: &CommsItem) -> &str {
    if !item.execution_phase.trim().is_empty() {
        return item.execution_phase.trim();
    }
    if !item.status_label.trim().is_empty() {
        return item.status_label.trim();
    }
    empty_dash(&item.status)
}

fn truncate_comms_preview(value: &str, width: usize) -> String {
    let text = value.trim();
    if width <= 3 {
        return text.chars().take(width).collect();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width.saturating_sub(3)).collect();
    format!("{head}...")
}

fn comms_reason(item: &CommsItem) -> Option<String> {
    if comms_is_normal_terminal(item) {
        return None;
    }
    if let Some(diagnostic) = item.active_inbound_diagnostic.as_ref() {
        let condition = diagnostic.condition_kind.trim();
        let reason = diagnostic.reason.trim();
        if !condition.is_empty() && !reason.is_empty() {
            return Some(format!("{condition}:{reason}"));
        }
        if !condition.is_empty() {
            return Some(condition.to_string());
        }
    }
    item.execution_phase_reason
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            item.block_reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            item.short_reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(str::to_string)
}

fn comms_is_normal_terminal(item: &CommsItem) -> bool {
    matches!(item.execution_phase.trim(), "terminal")
        || matches!(item.business_status.trim(), "replied" | "completed")
        || matches!(item.status_label.trim(), "done")
}

fn comms_cancel_enabled(item: &CommsItem) -> bool {
    if let Some(enabled) = item.cancel_enabled {
        return enabled;
    }
    if item.recoverable {
        return true;
    }
    matches!(
        item.status.trim(),
        "accepted" | "queued" | "running" | "failed" | "incomplete" | "cancelled"
    ) || matches!(
        item.business_status.trim(),
        "sending"
            | "delivering"
            | "blocked"
            | "replying"
            | "failed"
            | "delivery_failed"
            | "incomplete"
    ) || matches!(
        item.status_label.trim(),
        "send" | "back" | "work" | "stuck" | "fail"
    )
}

#[cfg(test)]
fn comms_status_color(item: &CommsItem) -> Color {
    comms_status_color_with_theme(item, SidebarTheme::default_dark())
}

fn comms_status_color_with_theme(item: &CommsItem, theme: SidebarTheme) -> Color {
    match item.execution_phase.trim() {
        "queued"
        | "injecting"
        | "provider_idle_pending_terminal"
        | "reply_queued"
        | "reply_delivering" => return theme.warning,
        "executing" => return theme.success,
        "orphaned" => return theme.danger,
        "terminal" => return theme.info,
        "unknown" => return theme.neutral,
        _ => {}
    }
    match item.business_status.trim() {
        "sending" | "delivering" | "blocked" => theme.warning,
        "replying" => theme.success,
        "replied" | "completed" => theme.info,
        "failed" | "delivery_failed" | "incomplete" | "cancelled" => theme.danger,
        _ => match item.status_label.trim() {
            "send" | "back" => theme.warning,
            "stuck" => theme.warning,
            "work" => theme.success,
            "done" => theme.info,
            "fail" => theme.danger,
            _ => theme.neutral,
        },
    }
}

fn is_terminal_cancel_error(error: &str) -> bool {
    let value = error.trim().to_lowercase();
    value.contains("already terminal")
}

fn recover_job_id(item: &CommsItem) -> Option<&str> {
    item.recover_target
        .as_ref()
        .and_then(|value| value.get("job_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn recover_reply_delivery_job_id(item: &CommsItem) -> Option<&str> {
    item.recover_target
        .as_ref()
        .and_then(|value| value.get("reply_delivery_job_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NamespaceInfo, ProjectInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use serde_json::json;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use std::thread;

    #[test]
    fn selection_tracks_project_view_rows() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
        app.move_selection(-1);
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Window("main".into()))
        );
        app.move_selection(1);
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
        assert_eq!(app.namespace_epoch(), Some(1));
    }

    #[test]
    fn selection_follows_active_agent_after_refresh() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_two_agents();
        response.view.agents[0].active = true;
        response.view.agents[1].active = false;
        app.apply_response(response);

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );

        let mut response = sample_response_with_two_agents();
        response.view.agents[0].active = false;
        response.view.agents[1].active = true;
        app.apply_response(response);

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent2".into()))
        );
    }

    #[test]
    fn manual_selection_is_preserved_until_focus_is_requested() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_two_agents());

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
        app.move_selection(1);
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent2".into()))
        );

        let mut response = sample_response_with_two_agents();
        response.view.agents[0].active = true;
        response.view.agents[1].active = false;
        app.apply_response(response);

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent2".into()))
        );
    }

    #[test]
    fn selection_falls_back_to_active_window_when_no_agent_is_active() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_two_agents();
        response.view.agents[0].active = false;
        response.view.agents[1].active = false;
        app.apply_response(response);

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Window("main".into()))
        );
    }

    #[test]
    fn selection_falls_back_to_namespace_window() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_two_agents();
        response.view.windows[0].active = false;
        response.view.agents[0].active = false;
        response.view.agents[1].active = false;
        response.view.namespace.active_window = Some("main".into());
        app.apply_response(response);

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Window("main".into()))
        );
    }

    #[test]
    fn keyboard_selection_can_move_from_focus_synced_agent() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());

        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
        app.move_selection(-1);
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Window("main".into()))
        );
        app.move_selection(1);
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
    }

    #[test]
    fn refresh_interval_respects_project_view_ttl_for_focus_following() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());

        assert_eq!(app.refresh_interval(), Duration::from_millis(1000));
    }

    #[test]
    fn mouse_coordinates_map_to_tree_targets_only() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);

        assert_eq!(app.target_index_at(1, 1, area), Some(0));
        assert_eq!(app.target_index_at(1, 2, area), Some(1));
        assert_eq!(app.target_index_at(0, 1, area), None);
        assert_eq!(app.target_index_at(1, 0, area), None);
        assert_eq!(app.target_index_at(1, 8, area), None);
    }

    #[test]
    fn renders_project_view_tree_and_comms() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        app.move_selection(1);

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(!rendered.contains("repo · main"));
        assert!(rendered.contains("> main"));
        assert!(rendered.contains("⚙ ×"));
        assert!(!rendered.contains("@1"));
        assert!(rendered.contains("◐* agent1 [codex]"));
        assert!(!rendered.contains("#job1"));
        assert!(rendered.contains("Comms"));
        assert!(rendered.contains("↻  X  ⌫  agent2 > agent1 #msg1 run"));

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray);
        let controls = tree_controls_area(Rect::new(0, 0, 80, 14));
        assert_eq!(controls, Rect::new(76, 0, 3, 1));
        assert_eq!(buffer[(controls.x, 0)].symbol(), "⚙");
        assert_eq!(buffer[(controls.x, 0)].fg, Color::Blue);
        assert_eq!(buffer[(controls.x + 2, 0)].symbol(), "×");
        assert_eq!(buffer[(controls.x + 2, 0)].fg, Color::Red);
        let symbol_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "◐")
            .expect("pending status symbol should render");
        assert_eq!(symbol_cell.fg, Color::Yellow);
        let status_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "r")
            .expect("comms status should render");
        assert_eq!(status_cell.fg, Color::Green);
        let retry_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "↻" && cell.fg == Color::Yellow)
            .expect("retry action should render");
        assert_eq!(retry_cell.fg, Color::Yellow);
        let cancel_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "X")
            .expect("cancel action should render");
        assert_eq!(cancel_cell.fg, Color::Red);
        let clear_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "⌫")
            .expect("clear action should render");
        assert_eq!(clear_cell.fg, Color::Cyan);
    }

    #[test]
    fn renders_display_names_while_focus_keeps_canonical_agent_name() {
        let mut response = sample_response();
        response.view.agents[0].display_name = "Main Claude".into();
        response.view.agents[0].provider_display_name = "Claude".into();
        let mut app = SidebarApp::new("main".into());
        app.apply_response(response);

        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("◐* Main Claude [Claude]"));
        assert_eq!(
            app.selected_target(),
            Some(RowTarget::Agent("agent1".into()))
        );
    }

    #[test]
    fn renders_provider_auth_recovery_action() {
        let mut response = sample_response();
        response.view.agents[0].activity_state = "failed".into();
        response.view.agents[0].activity_symbol = Some("✕".into());
        response.view.agents[0].activity_color = Some("red".into());
        response.view.agents[0].reason = Some("provider_auth_revoked".into());
        let mut app = SidebarApp::new("main".into());
        app.apply_response(response);

        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("✕* agent1 [login]"));
    }

    #[test]
    fn renders_reload_drain_agent_status() {
        let mut response = sample_response_with_two_agents();
        let agent = response
            .view
            .agents
            .iter_mut()
            .find(|agent| agent.name == "agent2")
            .expect("sample response should include agent2");
        agent.dispatch_blocked_by_reload_drain = true;
        agent.reload_drain = Some(crate::model::ReloadDrainView {
            intent_kind: "unload".into(),
            phase: "draining".into(),
            status: "waiting".into(),
        });

        let mut app = SidebarApp::new("main".into());
        app.apply_response(response);

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("◐  agent2 [claude] drain:waiting"));
        assert!(!rendered.contains("●  agent2 [claude]"));
    }

    #[test]
    fn tree_controls_render_as_right_aligned_symbol_pair() {
        let line = tree_controls_line(SidebarTheme::default_dark());
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "⚙ ×");
        assert_eq!(
            tree_controls_area(Rect::new(0, 0, 23, 24)),
            Rect::new(19, 0, 3, 1)
        );
    }

    #[test]
    fn light_theme_uses_readable_sidebar_semantic_colors() {
        let theme = SidebarTheme::light();
        let app = SidebarApp::with_theme("main".into(), theme);
        let item = crate::model::CommsItem {
            business_status: "replying".into(),
            status_label: "work".into(),
            ..Default::default()
        };
        let controls = tree_controls_line(theme);
        let action_spans = comms_action_spans(&item, theme);

        assert_eq!(tree_focus_style(&app).fg, Some(Color::Rgb(108, 111, 133)));
        assert_eq!(controls.spans[0].style.fg, Some(Color::Rgb(30, 102, 245)));
        assert_eq!(controls.spans[2].style.fg, Some(Color::Rgb(210, 15, 57)));
        assert_eq!(action_spans[0].style.fg, Some(Color::Rgb(223, 142, 29)));
        assert_eq!(
            comms_status_color_with_theme(&item, theme),
            Color::Rgb(64, 160, 43)
        );
    }

    #[test]
    fn ccb_kill_prefers_sibling_cli_binary() {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-sibling-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ccb = dir.join("ccb");
        std::fs::write(&ccb, b"#!/bin/sh\n").unwrap();
        let sidebar = dir.join("ccb-agent-sidebar");

        assert_eq!(ccb_program_for_sidebar(&sidebar), Some(ccb));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_ui_command_is_project_scoped() {
        let project_root = Path::new("/repo/demo");
        let command = config_ui_command(project_root, Path::new("/opt/ccb/bin/ccb"));

        assert_eq!(command.get_program(), "/opt/ccb/bin/ccb");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--project", "/repo/demo", "config", "ui"]
        );
        assert_eq!(command.get_current_dir(), Some(project_root));
    }

    #[test]
    fn ccb_program_resolution_finds_release_root_above_bin() {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-program-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sidebar = dir.join("bin/ccb-agent-sidebar");
        let ccb = dir.join("ccb");
        std::fs::create_dir_all(sidebar.parent().unwrap()).unwrap();
        std::fs::write(&sidebar, b"sidebar").unwrap();
        std::fs::write(&ccb, b"ccb").unwrap();

        assert_eq!(ccb_program_for_sidebar(&sidebar), Some(ccb));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn config_ui_launch_reports_ready_url_and_late_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-config-ui-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("ready-ccb");
        std::fs::write(
            &ready,
            b"#!/bin/sh\nprintf 'config_ui_status: serving\\nurl: http://127.0.0.1:43123/?token=test\\n'\nsleep 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&ready, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launch = launch_config_ui(&dir, &ready).unwrap();

        let ready_status = wait_for_config_ui_status(&launch, |status| {
            matches!(status, ConfigUiLaunchStatus::Ready(_))
        });
        assert_eq!(
            ready_status,
            ConfigUiLaunchStatus::Ready("http://127.0.0.1:43123/?token=test".to_string())
        );

        let manual = dir.join("manual-ccb");
        std::fs::write(
            &manual,
            b"#!/bin/sh\nprintf 'url: http://127.0.0.1:43124/?token=manual\\nbrowser_open: failed; open the URL above manually\\n'\nsleep 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&manual, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launch = launch_config_ui(&dir, &manual).unwrap();
        let manual_status = wait_for_config_ui_status(&launch, |status| {
            matches!(status, ConfigUiLaunchStatus::Manual(_))
        });
        assert_eq!(
            manual_status,
            ConfigUiLaunchStatus::Manual("http://127.0.0.1:43124/?token=manual".to_string())
        );

        let failed = dir.join("failed-ccb");
        std::fs::write(
            &failed,
            b"#!/bin/sh\nprintf 'browser unavailable\\n' >&2\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&failed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launch = launch_config_ui(&dir, &failed).unwrap();
        let failed_status = wait_for_config_ui_status(&launch, |status| {
            matches!(status, ConfigUiLaunchStatus::Failed(_))
        });
        assert_eq!(
            failed_status,
            ConfigUiLaunchStatus::Failed("browser unavailable".to_string())
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    fn wait_for_config_ui_status(
        launch: &ConfigUiLaunch,
        predicate: impl Fn(&ConfigUiLaunchStatus) -> bool,
    ) -> ConfigUiLaunchStatus {
        for _ in 0..100 {
            let status = launch.lock().unwrap().clone();
            if predicate(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        launch.lock().unwrap().clone()
    }

    #[cfg(unix)]
    #[test]
    fn project_kill_runs_ccb_kill_from_project_root() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-kill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project_root = dir.join("repo");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        let marker = dir.join("marker");
        let ccb = bin_dir.join("ccb");
        std::fs::write(
            &ccb,
            format!(
                "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$1\" > {}\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&ccb).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&ccb, permissions).unwrap();

        run_ccb_kill_with_program(ccb, &project_root).unwrap();

        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            format!("{}|kill\n", project_root.display())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renders_sidebar_config_error_without_dropping_comms_actions() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response();
        response.view.namespace.sidebar.view_error =
            Some("invalid TOML config: Unclosed array".into());
        app.apply_response(response);

        let rendered = render_to_string(&app, 80, 36);

        assert!(rendered.contains("config ✕"));
        assert!(rendered.contains("config error: invalid TOML config"));
        assert!(rendered.contains("↻  X  ⌫  agent2 > agent1 #msg1 run"));
    }

    #[test]
    fn renders_three_panel_sidebar_with_default_compact_comms_rows() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_comms(6));

        let backend = TestBackend::new(80, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("↻  X  ⌫  agent1 > agent1 #msg1 ok"));
        assert!(rendered.contains("↻  X  ⌫  agent2 > agent1 #msg2 ok"));
        assert!(!rendered.contains("agent3 > agent1"));
        assert!(!rendered.contains("agent4 > agent1"));
        assert!(!rendered.contains("agent5 > agent1"));
        assert!(!rendered.contains("agent6 > agent1"));
        assert!(rendered.contains("Tips"));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 18)].symbol(), "┌");
        assert_eq!(buffer[(1, 18)].symbol(), "C");
        assert_eq!(buffer[(0, 24)].symbol(), "┌");
        assert_eq!(buffer[(1, 24)].symbol(), "T");
    }

    #[test]
    fn tall_sidebar_uses_default_fifty_fifteen_and_thirty_five_split() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_comms(6));
        let area = Rect::new(0, 0, 24, 40);

        let areas = sidebar_areas(area, app.sidebar_view());

        assert_eq!(areas.tree.height, 20);
        assert_eq!(areas.comms.height, 6);
        assert_eq!(areas.tips.map(|area| area.height), Some(14));
    }

    #[test]
    fn configured_sidebar_view_can_adjust_all_three_sections() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_comms(6);
        response.view.namespace.sidebar.view.agents_height =
            serde_json::Value::String("40%".into());
        response.view.namespace.sidebar.view.comms_height = serde_json::Value::String("20%".into());
        response.view.namespace.sidebar.view.tips_height = serde_json::Value::String("40%".into());
        app.apply_response(response);
        let area = Rect::new(0, 0, 24, 40);

        let areas = sidebar_areas(area, app.sidebar_view());

        assert_eq!(areas.tree.height, 16);
        assert_eq!(areas.comms.height, 8);
        assert_eq!(areas.tips.map(|area| area.height), Some(16));
    }

    #[test]
    fn tree_area_scrolls_independently_and_clicks_scrolled_targets() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_agents(8));
        let area = Rect::new(0, 0, 40, 18);
        app.tree_scroll = 2;

        let rendered = render_to_string(&app, 40, 18);

        assert!(!rendered.contains("◐* agent1 [codex]"));
        assert!(rendered.contains("agent8 [codex]"));
        assert_eq!(app.target_index_at(1, 1, area), Some(2));
    }

    #[test]
    fn comms_area_scrolls_independently_and_clicks_scrolled_items() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_comms(8));
        let area = Rect::new(0, 0, 80, 36);
        app.comms_scroll = 2;

        let rendered = render_to_string(&app, 80, 36);

        assert!(!rendered.contains("agent1 > agent1 #msg1 ok"));
        assert!(rendered.contains("agent3 > agent1 #msg3 ok"));
        assert_eq!(
            app.comms_index_at(1, comms_row_y(&app, area, 0), area),
            Some(2)
        );
    }

    #[test]
    fn mouse_wheel_routes_to_panel_under_cursor() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_agents(24);
        response.view.comms = sample_response_with_comms(8).view.comms;
        response.view.namespace.sidebar.view.tips =
            (1..=20).map(|index| format!("tip {index}")).collect();
        app.apply_response(response);
        let area = Rect::new(0, 0, 80, 36);
        let areas = app.sidebar_areas(area);

        assert!(app.scroll_panel_at(1, areas.tree.y + 1, area, 1));
        assert_eq!(app.tree_scroll, 1);
        assert!(app.scroll_panel_at(1, areas.comms.y + 1, area, 1));
        assert_eq!(app.comms_scroll, 1);
        let tips = areas.tips.expect("tips area should render");
        assert!(app.scroll_panel_at(1, tips.y + 1, area, 1));
        assert_eq!(app.tips_scroll, 1);
    }

    #[test]
    fn dragging_dividers_resizes_all_three_sections() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response_with_comms(6));
        let area = Rect::new(0, 0, 24, 40);
        let areas = app.sidebar_areas(area);

        assert_eq!(areas.tree.height, 20);
        assert_eq!(areas.comms.height, 6);
        assert_eq!(areas.tips.map(|area| area.height), Some(14));
        assert_eq!(
            app.resize_divider_at(1, areas.comms.y, area),
            Some(SidebarResizeDivider::TreeComms)
        );
        assert!(app.start_resize_at(1, areas.comms.y, area));
        assert!(app.handle_mouse_drag(1, areas.comms.y + 4, area));

        let resized = app.sidebar_areas(area);
        assert_eq!(resized.tree.height, 23);
        assert_eq!(resized.comms.height, 3);
        assert_eq!(resized.tips.map(|area| area.height), Some(14));

        app.handle_mouse_up();
        let tips = resized.tips.expect("tips area should render");
        assert_eq!(
            app.resize_divider_at(1, tips.y, area),
            Some(SidebarResizeDivider::CommsTips)
        );
        assert!(app.start_resize_at(1, tips.y, area));
        assert!(app.handle_mouse_drag(1, tips.y + 4, area));

        let resized = app.sidebar_areas(area);
        assert_eq!(resized.tree.height, 23);
        assert_eq!(resized.comms.height, 7);
        assert_eq!(resized.tips.map(|area| area.height), Some(10));
    }

    #[test]
    fn tree_header_marks_focus_in_another_window() {
        let mut app = SidebarApp::new("review".into());
        app.apply_response(sample_response());

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("focus:main"));
        assert!(!rendered.contains("repo · review"));
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::DarkGray);
    }

    #[test]
    fn narrow_tree_header_prioritizes_cross_window_focus() {
        let mut app = SidebarApp::new("review".into());
        app.apply_response(sample_response());

        let rendered = render_to_string(&app, 24, 14);

        assert!(rendered.contains("focus:main"));
        assert!(!rendered.contains("review>main"));
    }

    #[test]
    fn rpc_failure_keeps_last_good_project_view_and_marks_degraded() {
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        app.set_error("connect /tmp/ccbd.sock: refused".into());

        let rendered = render_to_string(&app, 80, 36);

        assert!(rendered.contains("ccbd ✕"));
        assert!(!rendered.contains("repo · main"));
        assert!(rendered.contains("> main"));
        assert!(rendered.contains("◐* agent1 [codex]"));
        assert!(!rendered.contains("#job1"));
        assert!(rendered.contains("stale ProjectView"));
        assert!(rendered.contains("↻  X  ⌫  agent2 > agent1 #msg1 run"));
        assert!(!rendered.contains("connect /tmp/ccbd.sock"));
    }

    #[test]
    fn rpc_failure_without_last_good_view_renders_minimal_degraded_screen() {
        let mut app = SidebarApp::new("main".into());
        app.set_error("empty response from ccbd".into());

        let rendered = render_to_string(&app, 80, 14);

        assert!(rendered.contains("ccbd ✕"));
        assert!(!rendered.contains("CCB · main"));
        assert!(rendered.contains("ccbd unavailable"));
        assert!(!rendered.contains("agent1"));
        assert!(!rendered.contains("empty response from ccbd"));
    }

    #[test]
    fn rpc_failure_backoff_extends_after_first_failure_and_resets_on_success() {
        let mut app = SidebarApp::new("main".into());

        app.set_error("first".into());
        assert_eq!(app.failure_count, 1);
        assert!(!app.needs_refresh());

        app.set_error("second".into());
        assert_eq!(app.failure_count, 2);
        assert!(!app.needs_refresh());

        app.apply_response(sample_response());
        assert_eq!(app.failure_count, 0);
    }

    #[test]
    fn comms_line_includes_short_id_and_reason_when_present() {
        let item = crate::model::CommsItem {
            short_id: "abcd".into(),
            sender: "agent2".into(),
            target: "agent1".into(),
            status: "failed".into(),
            business_status: "failed".into(),
            status_label: "fail".into(),
            body_preview: "check agent status".into(),
            short_reason: Some("timeout".into()),
            ..Default::default()
        };

        assert_eq!(
            comms_line_text(&item),
            "↻  X  ⌫  agent2 > agent1 #abcd err\n   check agent status timeout"
        );
        assert_eq!(comms_status_color(&item), Color::Red);
    }

    #[test]
    fn comms_line_prefers_execution_phase_with_legacy_fallback_available() {
        let item = crate::model::CommsItem {
            sender: "agent2".into(),
            target: "agent1".into(),
            status: "running".into(),
            business_status: "failed".into(),
            status_label: "fail".into(),
            execution_phase: "executing".into(),
            execution_phase_reason: Some("provider_active".into()),
            body_preview: "work".into(),
            ..Default::default()
        };

        assert_eq!(
            comms_line_text(&item),
            "↻  X  ⌫  agent2 > agent1 run\n   work provider_active"
        );
        assert_eq!(comms_status_color(&item), Color::Green);

        let legacy = crate::model::CommsItem {
            execution_phase: String::new(),
            execution_phase_reason: None,
            ..item
        };
        assert!(comms_line_text(&legacy).contains("agent2 > agent1 err"));
    }

    #[test]
    fn comms_reason_prefers_orphaned_active_inbound_envelope() {
        let item = crate::model::CommsItem {
            execution_phase: "orphaned".into(),
            execution_phase_reason: Some("provider_idle_without_terminal".into()),
            active_inbound_diagnostic: Some(crate::model::ActiveInboundDiagnostic {
                condition_kind: "orphaned_active_inbound".into(),
                reason: "provider_idle_without_terminal".into(),
                recommended_action: "explicit_comms_recover".into(),
                automatic_action: "none".into(),
                ..Default::default()
            }),
            recoverable: true,
            ..Default::default()
        };

        assert_eq!(
            comms_reason(&item).as_deref(),
            Some("orphaned_active_inbound:provider_idle_without_terminal")
        );
        assert!(item.recoverable);
        assert_eq!(
            item.active_inbound_diagnostic
                .as_ref()
                .map(|diagnostic| diagnostic.automatic_action.as_str()),
            Some("none")
        );
    }

    #[test]
    fn compact_comms_rows_use_two_lines_with_message_under_cancel_column() {
        let item = crate::model::CommsItem {
            sender: "aaa".into(),
            target: "bbb".into(),
            status: "completed".into(),
            business_status: "replied".into(),
            status_label: "done".into(),
            body_preview: "message content".into(),
            ..Default::default()
        };
        let rendered = comms_lines(&item, 32, true)
            .into_iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered.as_slice(),
            ["↻  X  ⌫  aaa > bbb ok", "   message content"]
        );
    }

    #[test]
    fn comms_line_marks_recoverable_items_with_block_reason() {
        let item = crate::model::CommsItem {
            id: "job1".into(),
            short_id: "job1".into(),
            sender: "agent2".into(),
            target: "agent1".into(),
            status: "running".into(),
            business_status: "replying".into(),
            status_label: "work".into(),
            body_preview: "check agent status".into(),
            recoverable: true,
            block_reason: Some("pane_dead".into()),
            recover_target: Some(json!({"job_id": "job1", "reply_delivery_job_id": "job2"})),
            ..Default::default()
        };

        assert_eq!(
            comms_line_text(&item),
            "↻  X  ⌫  agent2 > agent1 #job1 run\n   check agent status pane_dead"
        );
        assert_eq!(recover_job_id(&item), Some("job1"));
        assert_eq!(recover_reply_delivery_job_id(&item), Some("job2"));
    }

    #[test]
    fn comms_line_hides_reason_for_normal_terminal_rows() {
        let item = crate::model::CommsItem {
            short_id: "abcd".into(),
            sender: "agent2".into(),
            target: "agent1".into(),
            status: "completed".into(),
            business_status: "replied".into(),
            status_label: "done".into(),
            body_preview: "all set".into(),
            short_reason: Some("hook_stop".into()),
            ..Default::default()
        };

        assert_eq!(
            comms_line_text(&item),
            "↻  X  ⌫  agent2 > agent1 #abcd ok\n   all set"
        );
        assert_eq!(comms_status_color(&item), Color::Blue);
    }

    #[test]
    fn comms_preview_truncates_to_available_width() {
        let item = crate::model::CommsItem {
            sender: "agent2".into(),
            target: "agent1".into(),
            status_label: "done".into(),
            body_preview: "COMMS_BUSINESS_VIEW_OK".into(),
            ..Default::default()
        };
        let rendered = comms_lines(&item, 12, false)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered.as_slice(),
            ["↻  X  ⌫  agent2 > agent1 ok", "COMMS_BUS..."]
        );
    }

    #[test]
    fn compact_comms_preview_truncates_to_available_width_on_second_line() {
        let item = crate::model::CommsItem {
            sender: "agent2".into(),
            target: "agent1".into(),
            status_label: "done".into(),
            body_preview: "COMMS_BUSINESS_VIEW_OK".into(),
            ..Default::default()
        };
        let rendered = comms_lines(&item, 35, true)
            .into_iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered.as_slice(),
            ["↻  X  ⌫  agent2 > agent1 ok", "   COMMS_BUSINESS_VIEW_OK"]
        );
        let narrow = comms_lines(&item, 10, true)
            .into_iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            narrow.as_slice(),
            ["↻  X  ⌫  agent2 > agent1 ok", "   COMM..."]
        );
    }

    #[test]
    fn mouse_coordinates_map_to_comms_rows() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response_with_comms(3);
        response.view.namespace.sidebar.view.agents_height =
            serde_json::Value::String("50%".into());
        response.view.namespace.sidebar.view.comms_height = serde_json::Value::String("40%".into());
        response.view.namespace.sidebar.view.tips_height = serde_json::Value::String("10%".into());
        response.view.comms[0].body_preview = "line two".into();
        response.view.comms[1].body_preview = "line two".into();
        app.apply_response(response);
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);
        let row0_detail = comms_row_y(&app, area, 1);
        let row1 = comms_row_y(&app, area, 2);
        let before_comms = sidebar_areas(area, app.sidebar_view())
            .comms
            .y
            .saturating_sub(1);

        assert_eq!(app.comms_index_at(1, row0, area), Some(0));
        assert_eq!(app.comms_index_at(1, row0_detail, area), Some(0));
        assert_eq!(app.comms_index_at(1, row1, area), Some(1));
        assert_eq!(app.comms_index_at(0, row0, area), None);
        assert_eq!(app.comms_index_at(1, before_comms, area), None);
    }

    #[cfg(unix)]
    #[test]
    fn header_buttons_are_right_aligned_and_kill_project() {
        let client = CcbdClient::new("/tmp/ccb-sidebar-unused.sock");
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);
        let controls = tree_controls_area(sidebar_areas(area, app.sidebar_view()).tree);
        let project_root = Path::new("/tmp");
        let ccb_program = Path::new("/bin/true");

        assert_eq!(controls, Rect::new(20, 0, 3, 1));
        assert_eq!(
            app.handle_mouse_down(1, 0, area, &client, project_root, ccb_program),
            None
        );
        assert_eq!(
            app.handle_mouse_down(controls.x, 0, area, &client, project_root, ccb_program,),
            None
        );
        assert_eq!(
            header_action_at(
                controls.x + 1,
                0,
                sidebar_areas(area, app.sidebar_view()).tree
            ),
            Some(HeaderMouseAction::Settings)
        );

        assert!(app.last_error.is_none());
        assert!(!app.needs_refresh());
        assert_eq!(
            app.handle_mouse_down(controls.x + 2, 0, area, &client, project_root, ccb_program,),
            Some(ExitAction::KillProject)
        );
    }

    #[cfg(unix)]
    #[test]
    fn header_settings_launch_failure_is_visible() {
        let client = CcbdClient::new("/tmp/ccb-sidebar-unused.sock");
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);
        let controls = tree_controls_area(sidebar_areas(area, app.sidebar_view()).tree);

        assert_eq!(
            app.handle_mouse_down(
                controls.x,
                0,
                area,
                &client,
                Path::new("/tmp"),
                Path::new("/definitely/missing/ccb"),
            ),
            None
        );

        assert!(
            app.last_error
                .as_deref()
                .is_some_and(|error| error.starts_with("config ui launch failed:"))
        );
    }

    #[test]
    fn comms_mouse_action_columns_are_fixed() {
        let item = crate::model::CommsItem {
            id: "msg1".into(),
            sender: "agent2".into(),
            target: "agent1".into(),
            status_label: "work".into(),
            body_preview: "line two".into(),
            ..Default::default()
        };
        let area = Rect::new(0, 10, 24, 10);
        let theme = SidebarTheme::default_dark();

        assert_eq!(
            comms_action_at_area(&[item], area, (1, 11), 22, true, 0, theme),
            Some((0, CommsMouseAction::Retry))
        );
        assert_eq!(
            comms_action_at_area(
                &[sample_comms_item("msg1")],
                area,
                (3, 11),
                22,
                true,
                0,
                theme
            ),
            Some((0, CommsMouseAction::Select))
        );
        assert_eq!(
            comms_action_at_area(
                &[sample_comms_item("msg1")],
                area,
                (4, 11),
                22,
                true,
                0,
                theme
            ),
            Some((0, CommsMouseAction::Cancel))
        );
        assert_eq!(
            comms_action_at_area(
                &[sample_comms_item("msg1")],
                area,
                (7, 11),
                22,
                true,
                0,
                theme
            ),
            Some((0, CommsMouseAction::Clear))
        );
        assert_eq!(
            comms_action_at_area(
                &[sample_comms_item("msg1")],
                area,
                (10, 11),
                22,
                true,
                0,
                theme
            ),
            Some((0, CommsMouseAction::Select))
        );
        assert_eq!(
            comms_action_at_area(
                &[sample_comms_item("msg1")],
                area,
                (4, 12),
                22,
                true,
                0,
                theme
            ),
            Some((0, CommsMouseAction::Select))
        );
    }

    #[test]
    fn tips_rendering_respects_scroll_offset() {
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response();
        response.view.namespace.sidebar.view.tips = vec![
            "tip one".into(),
            "tip two".into(),
            "tip three".into(),
            "tip four".into(),
        ];
        app.tips_scroll = 2;
        app.apply_response(response);
        app.tips_scroll = 2;

        let rendered = render_to_string(&app, 40, 12);

        assert!(!rendered.contains("tip one"));
        assert!(rendered.contains("tip three"));
    }

    #[test]
    fn scrollbar_position_reaches_visual_end_at_max_scroll() {
        assert_eq!(scrollbar_position_for_scroll(0, 20, 10), 0);
        assert_eq!(scrollbar_position_for_scroll(10, 20, 10), 19);
        assert_eq!(scrollbar_position_for_scroll(5, 20, 10), 9);
        assert_eq!(scrollbar_position_for_scroll(0, 4, 10), 0);
    }

    #[cfg(unix)]
    #[test]
    fn clicking_plain_comms_row_is_consumed_for_future_details() {
        let client = CcbdClient::new("/tmp/not-used.sock");
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response();
        response.view.comms[0].recoverable = false;
        response.view.comms[0].target = "agent1".into();
        app.apply_response(response);
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);

        assert!(app.handle_comms_mouse_down(10, row0, area, &client));

        assert!(app.last_error.is_none());
        assert!(!app.needs_refresh());
        assert_eq!(app.selected_comms, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn clicking_retry_symbol_calls_ccbd() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_comms_recover_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response();
        response.view.comms[0].recoverable = true;
        response.view.comms[0].recover_target =
            Some(json!({"job_id": "job1", "reply_delivery_job_id": "job2"}));
        response.view.comms[0].block_reason = Some("provider_prompt_idle".into());
        app.apply_response(response);
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);

        assert!(app.handle_comms_mouse_down(1, row0, area, &client));
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(app.selected_comms, Some(0));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["comms_recover:job1:job2:provider_prompt_idle"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn clicking_cancel_symbol_calls_ccbd_cancel() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_cancel_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);

        assert!(app.handle_comms_mouse_down(4, row0, area, &client));
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(seen.lock().unwrap().as_slice(), ["cancel:msg1"]);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cancel_error_hides_comms_row_locally() {
        let (socket_path, handle) =
            spawn_error_server("cancel", "job is already terminal: completed");
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);

        assert!(app.handle_comms_mouse_down(4, row0, area, &client));
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(app.visible_comms(), Vec::<CommsItem>::new());
    }

    #[cfg(unix)]
    #[test]
    fn clicking_clear_symbol_dismisses_comms_through_ccbd() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_dismiss_comms_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        let area = Rect::new(0, 0, 24, 20);
        let row0 = comms_row_y(&app, area, 0);

        assert!(app.handle_comms_mouse_down(7, row0, area, &client));
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(app.visible_comms(), Vec::<CommsItem>::new());
        assert_eq!(seen.lock().unwrap().as_slice(), ["dismiss:msg1"]);
    }

    #[cfg(unix)]
    #[test]
    fn recover_first_visible_comms_calls_ccbd() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_comms_recover_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        let mut response = sample_response();
        response.view.comms[0].recoverable = true;
        response.view.comms[0].recover_target =
            Some(json!({"job_id": "job1", "reply_delivery_job_id": "job2"}));
        response.view.comms[0].block_reason = Some("provider_prompt_idle".into());
        app.apply_response(response);

        app.recover_first_visible_comms(&client);
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["comms_recover:job1:job2:provider_prompt_idle"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_view_focus_refreshes_and_retries_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_stale_focus_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        app.move_selection(1);

        app.focus_selected_target(&client);
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(app.namespace_epoch(), Some(2));
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            [
                "project_focus_agent:1",
                "project_view",
                "project_focus_agent:2"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn target_missing_focus_failure_does_not_refresh_or_retry() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_target_missing_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        app.move_selection(1);

        app.focus_selected_target(&client);
        handle.join().unwrap();

        assert_eq!(app.last_error.as_deref(), Some("target_missing"));
        assert_eq!(app.namespace_epoch(), Some(1));
        assert_eq!(seen.lock().unwrap().as_slice(), ["project_focus_agent"]);
    }

    #[cfg(unix)]
    #[test]
    fn enter_on_window_row_focuses_window_through_ccbd() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_window_focus_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("main".into());
        app.apply_response(sample_response());
        app.move_selection(-1);

        app.focus_selected_target(&client);
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["project_focus_window:main:1"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn tab_focuses_the_sidebar_window_through_ccbd() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (socket_path, handle) = spawn_window_focus_server(Arc::clone(&seen));
        let client = CcbdClient::new(socket_path);
        let mut app = SidebarApp::new("ops".into());
        app.apply_response(sample_response());

        app.focus_pane_window(&client);
        handle.join().unwrap();

        assert!(app.last_error.is_none());
        assert!(app.needs_refresh());
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["project_focus_window:ops:1"]
        );
    }

    fn sample_response() -> ProjectViewResponse {
        ProjectViewResponse {
            view: ProjectView {
                project: ProjectInfo {
                    display_name: "repo".into(),
                    root: "/repo".into(),
                },
                namespace: NamespaceInfo {
                    epoch: Some(1),
                    active_window: Some("main".into()),
                    entry_window: "main".into(),
                    ..NamespaceInfo::default()
                },
                windows: vec![WindowView {
                    name: "main".into(),
                    active: true,
                    tmux_window_id: Some("@1".into()),
                    ..WindowView::default()
                }],
                agents: vec![AgentView {
                    name: "agent1".into(),
                    provider: "codex".into(),
                    window: "main".into(),
                    active: true,
                    activity_state: "pending".into(),
                    activity_symbol: Some("◐".into()),
                    activity_color: Some("yellow".into()),
                    ..AgentView::default()
                }],
                comms: vec![crate::model::CommsItem {
                    id: "msg1".into(),
                    short_id: "msg1".into(),
                    sender: "agent2".into(),
                    target: "agent1".into(),
                    status: "running".into(),
                    business_status: "replying".into(),
                    status_label: "work".into(),
                    ..Default::default()
                }],
            },
            cache: Default::default(),
        }
    }

    fn sample_response_with_two_agents() -> ProjectViewResponse {
        let mut response = sample_response();
        response.view.windows[0].agents = vec!["agent1".into(), "agent2".into()];
        response.view.agents.push(AgentView {
            name: "agent2".into(),
            provider: "claude".into(),
            window: "main".into(),
            active: false,
            activity_state: "idle".into(),
            activity_symbol: Some("●".into()),
            activity_color: Some("green".into()),
            ..AgentView::default()
        });
        response
    }

    fn sample_response_with_agents(count: usize) -> ProjectViewResponse {
        let mut response = sample_response();
        response.view.windows[0].agents =
            (1..=count).map(|index| format!("agent{index}")).collect();
        response.view.agents = (1..=count)
            .map(|index| AgentView {
                name: format!("agent{index}"),
                provider: "codex".into(),
                window: "main".into(),
                active: index == 1,
                activity_state: if index == 1 {
                    "pending".into()
                } else {
                    "idle".into()
                },
                activity_symbol: Some(if index == 1 { "◐" } else { "●" }.into()),
                activity_color: Some(if index == 1 { "yellow" } else { "green" }.into()),
                ..AgentView::default()
            })
            .collect();
        response
    }

    fn sample_response_with_comms(count: usize) -> ProjectViewResponse {
        let mut response = sample_response();
        response.view.comms = (1..=count)
            .map(|index| crate::model::CommsItem {
                id: format!("msg{index}"),
                short_id: format!("msg{index}"),
                sender: format!("agent{index}"),
                target: "agent1".into(),
                status: "completed".into(),
                status_label: "done".into(),
                ..Default::default()
            })
            .collect();
        response
    }

    fn sample_comms_item(id: &str) -> CommsItem {
        CommsItem {
            id: id.into(),
            sender: "agent2".into(),
            target: "agent1".into(),
            status_label: "work".into(),
            body_preview: "line two".into(),
            ..Default::default()
        }
    }

    fn render_to_string(app: &SidebarApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    fn line_text(line: Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn spans_text(spans: &[Span<'static>]) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn comms_row_y(app: &SidebarApp, area: Rect, offset: u16) -> u16 {
        app.sidebar_areas(area)
            .comms
            .y
            .saturating_add(1)
            .saturating_add(offset)
    }

    #[cfg(unix)]
    fn spawn_stale_focus_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-stale-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                {
                    let mut reader = BufReader::new(&stream);
                    reader.read_line(&mut line).unwrap();
                }
                let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let op = request["op"].as_str().unwrap();
                let response = match (index, op) {
                    (0, "project_focus_agent") => {
                        seen.lock().unwrap().push(format!(
                            "{op}:{}",
                            request["request"]["namespace_epoch"].as_u64().unwrap()
                        ));
                        json!({"api_version": 2, "ok": false, "error": "stale_view"})
                    }
                    (1, "project_view") => {
                        seen.lock().unwrap().push(op.into());
                        project_view_response_with_epoch(2)
                    }
                    (2, "project_focus_agent") => {
                        seen.lock().unwrap().push(format!(
                            "{op}:{}",
                            request["request"]["namespace_epoch"].as_u64().unwrap()
                        ));
                        json!({"api_version": 2, "ok": true, "focus": {"kind": "agent"}})
                    }
                    _ => panic!("unexpected request {index}: {request}"),
                };
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .unwrap();
            }
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_target_missing_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-target-missing-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], "project_focus_agent");
            seen.lock().unwrap().push("project_focus_agent".into());
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": false, "error": "target_missing"})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_window_focus_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-window-focus-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], "project_focus_window");
            seen.lock().unwrap().push(format!(
                "project_focus_window:{}:{}",
                request["request"]["window"].as_str().unwrap(),
                request["request"]["namespace_epoch"].as_u64().unwrap()
            ));
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": true, "focus": {"kind": "window"}})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_comms_recover_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-comms-recover-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], "comms_recover");
            seen.lock().unwrap().push(format!(
                "comms_recover:{}:{}:{}",
                request["request"]["job_id"].as_str().unwrap(),
                request["request"]["reply_delivery_job_id"]
                    .as_str()
                    .unwrap(),
                request["request"]["block_reason"].as_str().unwrap_or("")
            ));
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": true, "status": "recovered"})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_cancel_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-cancel-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], "cancel");
            seen.lock().unwrap().push(format!(
                "cancel:{}",
                request["request"]["job_id"].as_str().unwrap()
            ));
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": true, "status": "cancelled"})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_dismiss_comms_server(
        seen: Arc<Mutex<Vec<String>>>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-dismiss-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], "project_view_dismiss_comms");
            seen.lock().unwrap().push(format!(
                "dismiss:{}",
                request["request"]["id"].as_str().unwrap()
            ));
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": true, "status": "dismissed"})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn spawn_error_server(
        op: &'static str,
        error: &'static str,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-error-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["op"], op);
            stream
                .write_all(
                    format!(
                        "{}\n",
                        json!({"api_version": 2, "ok": false, "error": error})
                    )
                    .as_bytes(),
                )
                .unwrap();
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }

    #[cfg(unix)]
    fn project_view_response_with_epoch(epoch: u64) -> serde_json::Value {
        json!({
            "api_version": 2,
            "ok": true,
            "view": {
                "project": {"display_name": "repo", "root": "/repo"},
                "namespace": {"epoch": epoch, "active_window": "main", "entry_window": "main"},
                "windows": [{"name": "main", "active": true, "tmux_window_id": "@1"}],
                "agents": [{
                    "name": "agent1",
                    "provider": "codex",
                    "window": "main",
                    "active": true,
                    "activity_state": "pending",
                    "activity_symbol": "◐",
                    "activity_color": "yellow"
                }],
                "comms": []
            },
            "cache": {"sequence": epoch, "ttl_ms": 1000}
        })
    }

    fn comms_item_fixture(id: &str, short_id: &str, status: &str) -> CommsItem {
        CommsItem {
            id: id.into(),
            short_id: short_id.into(),
            sender: "pm".into(),
            target: "agent1".into(),
            status: status.into(),
            business_status: status.into(),
            status_label: status.into(),
            body_preview: "R-099 hotfix payload".into(),
            ..Default::default()
        }
    }

    fn agent_fixture(
        name: &str,
        provider: &str,
        state: &str,
        symbol: &str,
        color: &str,
        queue_depth: u64,
        current_job_id: Option<&str>,
    ) -> AgentView {
        AgentView {
            name: name.into(),
            provider: provider.into(),
            window: "main".into(),
            active: state != "idle",
            activity_state: state.into(),
            activity_symbol: Some(symbol.into()),
            activity_color: Some(color.into()),
            queue_depth,
            current_job_id: current_job_id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn comms_lines_include_short_id() {
        let item = comms_item_fixture("job_7deaebaf8043", "8043", "injecting");
        let lines = comms_lines(&item, 80, true);
        let first = line_text(lines.into_iter().next().expect("first line"));
        assert!(
            first.contains("#8043"),
            "first line should include #<short_id>, got: {first}"
        );
        assert!(
            first.contains("inj"),
            "first line should still include status, got: {first}"
        );
        assert!(
            first.contains("pm > agent1"),
            "first line should still include route, got: {first}"
        );
    }

    #[test]
    fn comms_lines_fallback_to_id_suffix_when_short_id_empty() {
        let item = comms_item_fixture("job_7deaebaf8043", "", "injecting");
        let lines = comms_lines(&item, 80, true);
        let first = line_text(lines.into_iter().next().expect("first line"));
        assert!(
            first.contains("#8043"),
            "should fallback to last 4 of full id, got: {first}"
        );
    }

    #[test]
    fn compact_comms_lines_include_short_id() {
        let item = comms_item_fixture("job_7deaebaf8043", "8043", "completed");
        let lines = compact_comms_lines(&item, 80, SidebarTheme::default_dark());
        let first = line_text(lines.into_iter().next().expect("first line"));
        assert!(
            first.contains("#8043"),
            "compact first line should include #<short_id>, got: {first}"
        );
        assert!(
            first.contains("ok"),
            "compact first line should still include status, got: {first}"
        );
    }

    #[test]
    fn agent_row_shows_queue_depth_and_active_jobid() {
        let agent = agent_fixture(
            "agent1",
            "claude",
            "running",
            "◐",
            "yellow",
            3,
            Some("job_7deaebaf8043"),
        );
        let text = spans_text(&agent_row_spans(&agent, SidebarTheme::default_dark()));
        assert!(
            text.contains("·3"),
            "should include queue depth, got: {text}"
        );
        assert!(
            text.contains("#8043"),
            "should include active jobid short, got: {text}"
        );
        assert!(
            text.contains("[claude]"),
            "should still include provider tag, got: {text}"
        );
    }

    #[test]
    fn agent_row_omits_queue_depth_when_zero() {
        let agent = agent_fixture("agent1", "claude", "idle", "●", "green", 0, None);
        let text = spans_text(&agent_row_spans(&agent, SidebarTheme::default_dark()));
        assert!(
            !text.contains("·0"),
            "queue_depth=0 should not be shown, got: {text}"
        );
        assert!(
            !text.contains('#'),
            "no current_job_id should mean no # token, got: {text}"
        );
    }

    #[test]
    fn agent_row_shows_jobid_only_without_queue_depth() {
        let agent = agent_fixture(
            "agent1",
            "claude",
            "running",
            "◐",
            "yellow",
            0,
            Some("job_xx1f9de3"),
        );
        let text = spans_text(&agent_row_spans(&agent, SidebarTheme::default_dark()));
        assert!(
            text.contains("#9de3"),
            "should include last-4 of current_job_id, got: {text}"
        );
        assert!(!text.contains("·0"), "no queue depth marker, got: {text}");
    }
}
