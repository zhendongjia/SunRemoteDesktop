use std::path::PathBuf;

use anyhow::Result;
use eframe::egui;

use crate::config::{self, AppConfig};

pub fn run() -> Result<()> {
    let config_path = config::config_path();
    let app = AdminApp::load(config_path)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SunRemoteDesktop 管理")
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([620.0, 460.0]),
        ..Default::default()
    };

    eframe::run_native(
        "SunRemoteDesktop 管理",
        options,
        Box::new(|_creation_context| Ok(Box::new(app))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct AdminApp {
    config: AppConfig,
    config_path: PathBuf,
    new_user: String,
    selected_user: Option<usize>,
    status: String,
}

impl AdminApp {
    fn load(config_path: PathBuf) -> Result<Self> {
        let config = config::load_from(&config_path)?;
        Ok(Self {
            config,
            config_path,
            new_user: String::new(),
            selected_user: None,
            status: "配置已加载".to_string(),
        })
    }

    fn save(&mut self) {
        self.config.normalize();
        match config::save_to(&self.config_path, &self.config) {
            Ok(()) => self.status = format!("已保存：{}", self.config_path.display()),
            Err(error) => self.status = format!("保存失败：{error:#}"),
        }
    }

    fn add_user(&mut self) {
        let user = self.new_user.trim();
        if user.is_empty() {
            return;
        }
        if !self
            .config
            .allowed_users
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(user))
        {
            self.config.allowed_users.push(user.to_string());
            self.config.normalize();
            self.new_user.clear();
            self.status = "已添加用户，点击“保存配置”后生效".to_string();
        }
    }

    fn remove_selected_user(&mut self) {
        if let Some(index) = self.selected_user.take()
            && index < self.config.allowed_users.len()
        {
            self.config.allowed_users.remove(index);
            self.status = "已移除用户，点击“保存配置”后生效".to_string();
        }
    }
}

impl eframe::App for AdminApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("SunRemoteDesktop");
            ui.label("使用 SunRDP 传输本地桌面画面；远程操作会注入到当前本地桌面。");
            ui.add_space(8.0);

            egui::Grid::new("server-settings")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.config.enabled, "启用服务");
                    ui.end_row();
                    ui.label("监听地址");
                    ui.text_edit_singleline(&mut self.config.bind_address);
                    ui.end_row();
                    ui.label("端口");
                    ui.add(egui::DragValue::new(&mut self.config.port).range(1..=65535));
                    ui.end_row();
                    ui.label("帧率上限");
                    ui.add(egui::DragValue::new(&mut self.config.fps).range(1..=120));
                    ui.end_row();
                    ui.label("最大连接数");
                    ui.label("1（当前认证模型暂不允许并发连接）");
                    ui.end_row();
                    ui.checkbox(&mut self.config.allow_control, "允许远程控制键盘和鼠标");
                    ui.end_row();
                });

            ui.add_space(14.0);
            ui.separator();
            ui.heading("允许连接的本地账户");
            ui.label(
                "可填写用户名、DOMAIN\\用户名、电脑名\\用户名或 .\\用户名。空列表将拒绝所有连接。",
            );

            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    let users = self.config.allowed_users.clone();
                    for (index, user) in users.into_iter().enumerate() {
                        let selected = self.selected_user == Some(index);
                        if ui.selectable_label(selected, user).clicked() {
                            self.selected_user = Some(index);
                        }
                    }
                });

            ui.horizontal(|ui| {
                let response = ui.text_edit_singleline(&mut self.new_user);
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.add_user();
                }
                if ui.button("添加").clicked() {
                    self.add_user();
                }
                if ui.button("移除选中").clicked() {
                    self.remove_selected_user();
                }
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("保存配置").clicked() {
                    self.save();
                }
                if ui.button("重新加载").clicked() {
                    match config::load_from(&self.config_path) {
                        Ok(config) => {
                            self.config = config;
                            self.selected_user = None;
                            self.status = "配置已重新加载".to_string();
                        }
                        Err(error) => self.status = format!("加载失败：{error:#}"),
                    }
                }
            });
            ui.separator();
            ui.small(format!("配置文件：{}", self.config_path.display()));
            ui.small(&self.status);
        });
    }
}
