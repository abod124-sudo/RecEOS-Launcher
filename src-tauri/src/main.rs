// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Stella's Steam sign-in runs in a short-lived copy of the launcher, so
    // Steam stops showing Rec Room as soon as it is done (see stella_api).
    // Checked first: that copy must not start the app, or meet its
    // single-instance guard.
    if std::env::args().nth(1).as_deref() == Some(rec_eos_launcher_lib::stella_api::TICKET_HELPER_ARG) {
        std::process::exit(rec_eos_launcher_lib::stella_api::steam_ticket_helper());
    }
    rec_eos_launcher_lib::run()
}
