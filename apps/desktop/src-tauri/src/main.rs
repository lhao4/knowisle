// 发布构建时不要额外弹出控制台窗口，删掉这行会让用户看到一个黑框。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    knowisle_lib::run();
}
