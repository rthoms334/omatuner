//! Regenerate the keymap tables in docs/DESIGN.md §15 from `src/keymap.rs`:
//! `cargo run --example keymap_md > /tmp/keys.md`. Owner: WP-10a (output pasted by WP-13).
use tuner::keymap::{KeyContext, help_rows};

fn main() {
    for c in KeyContext::ALL {
        println!("#### {} (`{:?}`)\n", c.title(), c);
        println!("| Keys | Action |\n|---|---|");
        for (k, h) in help_rows(c) {
            println!("| `{}` | {} |", k.replace('|', "\\|").replace(" / ", "` / `"), h);
        }
        println!();
    }
}
