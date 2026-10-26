#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
# SlashIt dev server
# NO_AT_BRIDGE=1 prevents WebKitGTK segfault on non-GNOME desktops (i3, sway, etc)
export NO_AT_BRIDGE=1
cargo tauri dev "$@"
