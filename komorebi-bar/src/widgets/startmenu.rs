use crate::config::DisplayFormat;
use crate::render::RenderConfig;
use crate::selected_frame::SelectableFrame;
use crate::widgets::widget::BarWidget;
use eframe::egui::text::LayoutJob;
use eframe::egui::Align;
use eframe::egui::Context;
use eframe::egui::Label;
use eframe::egui::TextFormat;
use eframe::egui::Ui;
use serde::Deserialize;
use serde::Serialize;
use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT_KEYBOARD;
use windows::Win32::UI::Input::KeyboardAndMouse::KEYBDINPUT;
use windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LWIN;

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StartMenuConfig {
    /// Enable the Start menu widget
    pub enable: bool,
    /// Display format
    pub display: Option<DisplayFormat>,
    /// Start menu icon
    pub icon: Option<StartMenuIcon>,
}

impl From<StartMenuConfig> for StartMenu {
    fn from(value: StartMenuConfig) -> Self {
        Self {
            enable: value.enable,
            display: value.display.unwrap_or(DisplayFormat::Icon),
            icon: value.icon.unwrap_or(StartMenuIcon::YinYang),
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum StartMenuIcon {
    /// Yin Yang
    YinYang,
    /// Windows logo
    WindowsLogo,
}

pub struct StartMenu {
    pub enable: bool,
    display: DisplayFormat,
    icon: StartMenuIcon,
}

impl StartMenu {
    pub fn toggle_start_menu() {
        // Prepare the inputs
        let inputs = [
            // Press the left windows key
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_LWIN,
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0), // key down
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            // Release the left windows key
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_LWIN,
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP, // key up
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];

        // Send the inputs
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

impl BarWidget for StartMenu {
    fn render(&mut self, ctx: &Context, ui: &mut Ui, config: &mut RenderConfig) {
        if self.enable {
            let mut layout_job = LayoutJob::simple(
                match self.display {
                    DisplayFormat::Icon
                    | DisplayFormat::IconAndText
                    | DisplayFormat::TextAndIconOnSelected
                    | DisplayFormat::IconAndTextOnSelected => match self.icon {
                        StartMenuIcon::YinYang => egui_phosphor::regular::YIN_YANG.to_string(),
                        StartMenuIcon::WindowsLogo => {
                            egui_phosphor::regular::WINDOWS_LOGO.to_string()
                        }
                    },
                    DisplayFormat::Text => String::new(),
                },
                config.icon_font_id.clone(),
                ctx.style().visuals.selection.stroke.color,
                100.0,
            );

            if self.display == DisplayFormat::IconAndText
                || self.display == DisplayFormat::Text
                || self.display == DisplayFormat::TextAndIconOnSelected
                || self.display == DisplayFormat::IconAndTextOnSelected
            {
                layout_job.append(
                    &String::from("Start"),
                    10.0,
                    TextFormat {
                        font_id: config.text_font_id.clone(),
                        color: ctx.style().visuals.text_color(),
                        valign: Align::Center,
                        ..Default::default()
                    },
                );
            }

            config.apply_on_widget(false, ui, |ui| {
                if SelectableFrame::new(false)
                    .show(ui, |ui| ui.add(Label::new(layout_job).selectable(false)))
                    .clicked()
                {
                    Self::toggle_start_menu();
                }
            });
        }
    }
}
