//! 让新增迁移文件能触发重编译。
//!
//! `sqlx::migrate!` 在编译期把迁移文件内嵌进二进制；新增文件不会自动
//! 让宏重新展开，所以这里显式声明目录依赖。

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
