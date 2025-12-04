use config::MiguxConfig;

fn main() {
    let migux_cfg = MiguxConfig::from_file("migux.conf");
    migux_cfg.print();
}
