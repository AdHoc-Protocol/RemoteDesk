// The program of a computer: the host of src/lib.rs, started from a console or by a double click.

fn main() {
    rdp_host::os::init();
    rdp_host::os::main(rdp_host::session::run);
}
