// 릴리즈 빌드에서는 콘솔창 뜨지 않음
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyzer;
mod capture;
mod gui;

fn main() -> eframe::Result {
    gui::run()
}
