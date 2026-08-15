//! Print the console to stdout.
//!
//! The page is normally served by the API, but the browser tests need it as a
//! file — and so does anybody reviewing the design, who should not have to
//! stand up a cloud to look at a stylesheet.
//!
//!     cargo run -p velstra-cloud-console --bin velstra-console-page > console.html

fn main() {
    print!("{}", velstra_cloud_console::page());
}
