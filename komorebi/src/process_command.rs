use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use color_eyre::eyre::anyhow;
use color_eyre::Result;
use miow::pipe::connect;
use parking_lot::Mutex;
use uds_windows::UnixStream;

use komorebi_core::FocusFollowsMouseImplementation;
use komorebi_core::Rect;
use komorebi_core::SocketMessage;
use komorebi_core::StateQuery;

use crate::notify_subscribers;
use crate::window_manager;
use crate::window_manager::WindowManager;
use crate::windows_api::WindowsApi;
use crate::Notification;
use crate::NotificationEvent;
use crate::BORDER_OVERFLOW_IDENTIFIERS;
use crate::CUSTOM_FFM;
use crate::FLOAT_IDENTIFIERS;
use crate::MANAGE_IDENTIFIERS;
use crate::SUBSCRIPTION_PIPES;
use crate::TRAY_AND_MULTI_WINDOW_IDENTIFIERS;
use crate::WORKSPACE_RULES;

#[tracing::instrument]
pub fn listen_for_commands(wm: Arc<Mutex<WindowManager>>) {
    let listener = wm
        .lock()
        .command_listener
        .try_clone()
        .expect("could not clone unix listener");

    thread::spawn(move || {
        tracing::info!("listening");
        for client in listener.incoming() {
            match client {
                Ok(stream) => match wm.lock().read_commands(stream) {
                    Ok(()) => {}
                    Err(error) => tracing::error!("{}", error),
                },
                Err(error) => {
                    tracing::error!("{}", error);
                    break;
                }
            }
        }
    });
}

impl WindowManager {
    #[tracing::instrument(skip(self))]
    pub fn process_command(&mut self, message: SocketMessage) -> Result<()> {
        self.validate_virtual_desktop_id();

        match message {
            SocketMessage::Promote => self.promote_container_to_front()?,
            SocketMessage::FocusWindow(direction) => {
                self.focus_container_in_direction(direction)?;
            }
            SocketMessage::MoveWindow(direction) => {
                self.move_container_in_direction(direction)?;
            }
            SocketMessage::CycleFocusWindow(direction) => {
                self.focus_container_in_cycle_direction(direction)?;
            }
            SocketMessage::CycleMoveWindow(direction) => {
                self.move_container_in_cycle_direction(direction)?;
            }
            SocketMessage::StackWindow(direction) => self.add_window_to_container(direction)?,
            SocketMessage::UnstackWindow => self.remove_window_from_container()?,
            SocketMessage::CycleStack(direction) => {
                self.cycle_container_window_in_direction(direction)?;
            }
            SocketMessage::ToggleFloat => self.toggle_float()?,
            SocketMessage::ToggleMonocle => self.toggle_monocle()?,
            SocketMessage::ToggleMaximize => self.toggle_maximize()?,
            SocketMessage::ContainerPadding(monitor_idx, workspace_idx, size) => {
                self.set_container_padding(monitor_idx, workspace_idx, size)?;
            }
            SocketMessage::WorkspacePadding(monitor_idx, workspace_idx, size) => {
                self.set_workspace_padding(monitor_idx, workspace_idx, size)?;
            }
            SocketMessage::WorkspaceRule(_, id, monitor_idx, workspace_idx) => {
                {
                    let mut workspace_rules = WORKSPACE_RULES.lock();
                    workspace_rules.insert(id, (monitor_idx, workspace_idx));
                }

                self.enforce_workspace_rules()?;
            }
            SocketMessage::ManageRule(_, id) => {
                let mut manage_identifiers = MANAGE_IDENTIFIERS.lock();
                if !manage_identifiers.contains(&id) {
                    manage_identifiers.push(id);
                }
            }
            SocketMessage::FloatRule(_, id) => {
                let mut float_identifiers = FLOAT_IDENTIFIERS.lock();
                if !float_identifiers.contains(&id) {
                    float_identifiers.push(id);
                }
            }
            SocketMessage::AdjustContainerPadding(sizing, adjustment) => {
                self.adjust_container_padding(sizing, adjustment)?;
            }
            SocketMessage::AdjustWorkspacePadding(sizing, adjustment) => {
                self.adjust_workspace_padding(sizing, adjustment)?;
            }
            SocketMessage::MoveContainerToWorkspaceNumber(workspace_idx) => {
                self.move_container_to_workspace(workspace_idx, true)?;
            }
            SocketMessage::MoveContainerToMonitorNumber(monitor_idx) => {
                self.move_container_to_monitor(monitor_idx, true)?;
            }
            SocketMessage::SendContainerToWorkspaceNumber(workspace_idx) => {
                self.move_container_to_workspace(workspace_idx, false)?;
            }
            SocketMessage::SendContainerToMonitorNumber(monitor_idx) => {
                self.move_container_to_monitor(monitor_idx, false)?;
            }
            SocketMessage::TogglePause => {
                if self.is_paused {
                    tracing::info!("resuming");
                } else {
                    tracing::info!("pausing");
                }

                self.is_paused = !self.is_paused;
            }
            SocketMessage::ToggleTiling => {
                self.toggle_tiling()?;
            }
            SocketMessage::CycleFocusMonitor(direction) => {
                let monitor_idx = direction.next_idx(
                    self.focused_monitor_idx(),
                    NonZeroUsize::new(self.monitors().len())
                        .ok_or_else(|| anyhow!("there must be at least one monitor"))?,
                );

                self.focus_monitor(monitor_idx)?;
                self.update_focused_workspace(self.mouse_follows_focus)?;
            }
            SocketMessage::FocusMonitorNumber(monitor_idx) => {
                self.focus_monitor(monitor_idx)?;
                self.update_focused_workspace(self.mouse_follows_focus)?;
            }
            SocketMessage::Retile => self.retile_all()?,
            SocketMessage::FlipLayout(layout_flip) => self.flip_layout(layout_flip)?,
            SocketMessage::ChangeLayout(layout) => self.change_workspace_layout_default(layout)?,
            SocketMessage::ChangeLayoutCustom(path) => self.change_workspace_custom_layout(path)?,
            SocketMessage::WorkspaceLayoutCustom(monitor_idx, workspace_idx, path) => {
                self.set_workspace_layout_custom(monitor_idx, workspace_idx, path)?;
            }
            SocketMessage::WorkspaceTiling(monitor_idx, workspace_idx, tile) => {
                self.set_workspace_tiling(monitor_idx, workspace_idx, tile)?;
            }
            SocketMessage::WorkspaceLayout(monitor_idx, workspace_idx, layout) => {
                self.set_workspace_layout_default(monitor_idx, workspace_idx, layout)?;
            }
            SocketMessage::CycleFocusWorkspace(direction) => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                let monitor_idx = self.monitor_idx_from_current_pos().ok_or_else(|| {
                    anyhow!("there is no monitor associated with the current cursor position")
                })?;

                self.focus_monitor(monitor_idx)?;

                let focused_monitor = self
                    .focused_monitor()
                    .ok_or_else(|| anyhow!("there is no monitor"))?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();
                let workspaces = focused_monitor.workspaces().len();

                let workspace_idx = direction.next_idx(
                    focused_workspace_idx,
                    NonZeroUsize::new(workspaces)
                        .ok_or_else(|| anyhow!("there must be at least one workspace"))?,
                );

                self.focus_workspace(workspace_idx)?;
            }
            SocketMessage::FocusWorkspaceNumber(workspace_idx) => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                let monitor_idx = self.monitor_idx_from_current_pos().ok_or_else(|| {
                    anyhow!("there is no monitor associated with the current cursor position")
                })?;

                self.focus_monitor(monitor_idx)?;

                self.focus_workspace(workspace_idx)?;
            }
            SocketMessage::Stop => {
                tracing::info!(
                    "received stop command, restoring all hidden windows and terminating process"
                );
                self.restore_all_windows();
                std::process::exit(0)
            }
            SocketMessage::EnsureWorkspaces(monitor_idx, workspace_count) => {
                self.ensure_workspaces_for_monitor(monitor_idx, workspace_count)?;
            }
            SocketMessage::NewWorkspace => {
                self.new_workspace()?;
            }
            SocketMessage::WorkspaceName(monitor_idx, workspace_idx, name) => {
                self.set_workspace_name(monitor_idx, workspace_idx, name)?;
            }
            SocketMessage::State => {
                let state = serde_json::to_string_pretty(&window_manager::State::from(&*self))?;
                let mut socket =
                    dirs::home_dir().ok_or_else(|| anyhow!("there is no home directory"))?;
                socket.push("komorebic.sock");
                let socket = socket.as_path();

                let mut stream = UnixStream::connect(&socket)?;
                stream.write_all(state.as_bytes())?;
            }
            SocketMessage::Query(query) => {
                let response = match query {
                    StateQuery::FocusedMonitorIndex => self.focused_monitor_idx(),
                    StateQuery::FocusedWorkspaceIndex => self
                        .focused_monitor()
                        .ok_or_else(|| anyhow!("there is no monitor"))?
                        .focused_workspace_idx(),
                    StateQuery::FocusedContainerIndex => {
                        self.focused_workspace()?.focused_container_idx()
                    }
                    StateQuery::FocusedWindowIndex => {
                        self.focused_container()?.focused_window_idx()
                    }
                }
                .to_string();

                let mut socket =
                    dirs::home_dir().ok_or_else(|| anyhow!("there is no home directory"))?;
                socket.push("komorebic.sock");
                let socket = socket.as_path();

                let mut stream = UnixStream::connect(&socket)?;
                stream.write_all(response.as_bytes())?;
            }
            SocketMessage::ResizeWindow(direction, sizing) => {
                self.resize_window(direction, sizing, Option::from(50))?;
            }
            SocketMessage::FocusFollowsMouse(mut implementation, enable) => {
                if !CUSTOM_FFM.load(Ordering::SeqCst) {
                    tracing::warn!(
                        "komorebi was not started with the --ffm flag, so the komorebi implementation of focus follows mouse cannot be enabled; defaulting to windows implementation"
                    );
                    implementation = FocusFollowsMouseImplementation::Windows;
                }

                match implementation {
                    FocusFollowsMouseImplementation::Komorebi => {
                        if WindowsApi::focus_follows_mouse()? {
                            tracing::warn!(
                                "the komorebi implementation of focus follows mouse cannot be enabled while the windows implementation is enabled"
                            );
                        } else if enable {
                            self.focus_follows_mouse = Option::from(implementation);
                        } else {
                            self.focus_follows_mouse = None;
                            self.has_pending_raise_op = false;
                        }
                    }
                    FocusFollowsMouseImplementation::Windows => {
                        if let Some(FocusFollowsMouseImplementation::Komorebi) =
                            self.focus_follows_mouse
                        {
                            tracing::warn!(
                                "the windows implementation of focus follows mouse cannot be enabled while the komorebi implementation is enabled"
                            );
                        } else if enable {
                            WindowsApi::enable_focus_follows_mouse()?;
                            self.focus_follows_mouse =
                                Option::from(FocusFollowsMouseImplementation::Windows);
                        } else {
                            WindowsApi::disable_focus_follows_mouse()?;
                            self.focus_follows_mouse = None;
                        }
                    }
                }
            }
            SocketMessage::ToggleFocusFollowsMouse(mut implementation) => {
                if !CUSTOM_FFM.load(Ordering::SeqCst) {
                    tracing::warn!(
                        "komorebi was not started with the --ffm flag, so the komorebi implementation of focus follows mouse cannot be toggled; defaulting to windows implementation"
                    );
                    implementation = FocusFollowsMouseImplementation::Windows;
                }

                match implementation {
                    FocusFollowsMouseImplementation::Komorebi => {
                        if WindowsApi::focus_follows_mouse()? {
                            tracing::warn!(
                                "the komorebi implementation of focus follows mouse cannot be toggled while the windows implementation is enabled"
                            );
                        } else {
                            match self.focus_follows_mouse {
                                None => {
                                    self.focus_follows_mouse = Option::from(implementation);
                                    self.has_pending_raise_op = false;
                                }
                                Some(FocusFollowsMouseImplementation::Komorebi) => {
                                    self.focus_follows_mouse = None;
                                }
                                Some(FocusFollowsMouseImplementation::Windows) => {
                                    tracing::warn!("ignoring command that could mix different focus follows mouse implementations");
                                }
                            }
                        }
                    }
                    FocusFollowsMouseImplementation::Windows => {
                        if let Some(FocusFollowsMouseImplementation::Komorebi) =
                            self.focus_follows_mouse
                        {
                            tracing::warn!(
                                "the windows implementation of focus follows mouse cannot be toggled while the komorebi implementation is enabled"
                            );
                        } else {
                            match self.focus_follows_mouse {
                                None => {
                                    WindowsApi::enable_focus_follows_mouse()?;
                                    self.focus_follows_mouse = Option::from(implementation);
                                }
                                Some(FocusFollowsMouseImplementation::Windows) => {
                                    WindowsApi::disable_focus_follows_mouse()?;
                                    self.focus_follows_mouse = None;
                                }
                                Some(FocusFollowsMouseImplementation::Komorebi) => {
                                    tracing::warn!("ignoring command that could mix different focus follows mouse implementations");
                                }
                            }
                        }
                    }
                }
            }
            SocketMessage::ReloadConfiguration => {
                Self::reload_configuration();
            }
            SocketMessage::WatchConfiguration(enable) => {
                self.watch_configuration(enable)?;
            }
            SocketMessage::IdentifyBorderOverflow(_, id) => {
                let mut identifiers = BORDER_OVERFLOW_IDENTIFIERS.lock();
                if !identifiers.contains(&id) {
                    identifiers.push(id);
                }
            }
            SocketMessage::IdentifyTrayApplication(_, id) => {
                let mut identifiers = TRAY_AND_MULTI_WINDOW_IDENTIFIERS.lock();
                if !identifiers.contains(&id) {
                    identifiers.push(id);
                }
            }
            SocketMessage::ManageFocusedWindow => {
                self.manage_focused_window()?;
            }
            SocketMessage::UnmanageFocusedWindow => {
                self.unmanage_focused_window()?;
            }
            SocketMessage::InvisibleBorders(rect) => {
                self.invisible_borders = rect;
                self.retile_all()?;
            }
            SocketMessage::WorkAreaOffset(rect) => {
                self.work_area_offset = Option::from(rect);
                self.retile_all()?;
            }
            SocketMessage::QuickSave => {
                let workspace = self.focused_workspace()?;
                let resize = workspace.resize_dimensions();

                let mut quicksave_json = std::env::temp_dir();
                quicksave_json.push("komorebi.quicksave.json");

                let file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(quicksave_json)?;

                serde_json::to_writer_pretty(&file, &resize)?;
            }
            SocketMessage::QuickLoad => {
                let workspace = self.focused_workspace_mut()?;

                let mut quicksave_json = std::env::temp_dir();
                quicksave_json.push("komorebi.quicksave.json");

                let file = File::open(&quicksave_json).map_err(|_| {
                    anyhow!(
                        "no quicksave found at {}",
                        quicksave_json.display().to_string()
                    )
                })?;

                let resize: Vec<Option<Rect>> = serde_json::from_reader(file)?;

                workspace.set_resize_dimensions(resize);
                self.update_focused_workspace(false)?;
            }
            SocketMessage::Save(path) => {
                let workspace = self.focused_workspace_mut()?;
                let resize = workspace.resize_dimensions();

                let file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(path)?;

                serde_json::to_writer_pretty(&file, &resize)?;
            }
            SocketMessage::Load(path) => {
                let workspace = self.focused_workspace_mut()?;

                let file = File::open(&path)
                    .map_err(|_| anyhow!("no file found at {}", path.display().to_string()))?;

                let resize: Vec<Option<Rect>> = serde_json::from_reader(file)?;

                workspace.set_resize_dimensions(resize);
                self.update_focused_workspace(false)?;
            }
            SocketMessage::AddSubscriber(subscriber) => {
                let mut pipes = SUBSCRIPTION_PIPES.lock();
                let pipe_path = format!(r"\\.\pipe\{}", subscriber);
                let pipe = connect(&pipe_path).map_err(|_| {
                    anyhow!("the named pipe '{}' has not yet been created; please create it before running this command", pipe_path)
                })?;

                pipes.insert(subscriber, pipe);
            }
            SocketMessage::RemoveSubscriber(subscriber) => {
                let mut pipes = SUBSCRIPTION_PIPES.lock();
                pipes.remove(&subscriber);
            }
            SocketMessage::ToggleMouseFollowsFocus => {
                self.mouse_follows_focus = !self.mouse_follows_focus;
            }
        };

        tracing::info!("processed");
        Ok(())
    }

    #[tracing::instrument(skip(self, stream))]
    pub fn read_commands(&mut self, stream: UnixStream) -> Result<()> {
        let stream = BufReader::new(stream);
        for line in stream.lines() {
            let message = SocketMessage::from_str(&line?)?;

            if self.is_paused {
                return match message {
                    SocketMessage::TogglePause | SocketMessage::State | SocketMessage::Stop => {
                        Ok(self.process_command(message)?)
                    }
                    _ => {
                        tracing::trace!("ignoring while paused");
                        Ok(())
                    }
                };
            }

            self.process_command(message.clone())?;
            notify_subscribers(&serde_json::to_string(&Notification {
                event: NotificationEvent::Socket(message.clone()),
                state: (&*self).into(),
            })?)?;
        }

        Ok(())
    }
}
