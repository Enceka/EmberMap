// 桌面端入口；应用逻辑在 lib.rs，与 Android 共用。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    embermap_lib::run()
}
