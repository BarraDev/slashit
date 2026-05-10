#![allow(unused_variables)]

mod app;

#[path = "models/mod.rs"]
mod models;

#[path = "services/mod.rs"]
mod services;

#[path = "components/mod.rs"]
mod components;

#[path = "pages/mod.rs"]
mod pages;

use app::*;
use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}
