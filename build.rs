fn main() {
    // ui feature 关闭时（如 --no-default-features 的纯逻辑测试）不编译 Slint UI
    #[cfg(feature = "ui")]
    slint_build::compile("ui/app.slint").expect("slint compile failed");
}
