use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;
use slint::{ModelRc, SharedString, VecModel};

use crate::{AppWindow, ToastData};

#[derive(Clone)]
pub struct ToastItem {
    pub id: i32,
    pub message: String,
    pub toast_type: String,
}

pub fn update_toast_model(ui: &AppWindow, toasts: &[ToastItem]) {
    let toast_data: Vec<ToastData> = toasts
        .iter()
        .map(|t| ToastData {
            id: t.id,
            message: SharedString::from(&t.message),
            toast_type: SharedString::from(&t.toast_type),
            opacity: 1.0,
        })
        .collect();
    ui.set_toasts(ModelRc::new(VecModel::<ToastData>::from(toast_data)));
}

pub fn push_toast(
    toasts: &Arc<Mutex<Vec<ToastItem>>>,
    next_id: &Arc<Mutex<i32>>,
    ui: &AppWindow,
    message: &str,
    toast_type: &str,
) {
    let mut toasts_vec = toasts.lock().unwrap();
    let mut nid = next_id.lock().unwrap();
    *nid += 1;
    toasts_vec.push(ToastItem {
        id: *nid,
        message: message.to_string(),
        toast_type: toast_type.to_string(),
    });
    update_toast_model(ui, &toasts_vec);

    let dismiss_id = *nid;
    let toasts_clone = toasts.clone();
    let ui_weak = ui.as_weak();
    let dismiss_timer = slint::Timer::default();
    dismiss_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(3000),
        move || {
            if let Some(fui) = ui_weak.upgrade() {
                let mut tv = toasts_clone.lock().unwrap();
                if let Some(pos) = tv.iter().position(|t| t.id == dismiss_id) {
                    tv.remove(pos);
                }
                update_toast_model(&fui, &tv);
            }
        },
    );
    Box::leak(Box::new(dismiss_timer));
}