use ccb_agent_sidebar::args::{Args, usage};

fn main() {
    unsafe {
        std::env::remove_var("NO_COLOR");
    }

    let args = match Args::parse_env() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{err}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    if args.dump_json {
        match ccb_agent_sidebar::client::CcbdClient::from_args(&args)
            .and_then(|client| client.project_view_json())
        {
            Ok(value) => {
                println!("{value}");
                return;
            }
            Err(error) => {
                eprintln!("ccb-agent-sidebar: {error}");
                std::process::exit(1);
            }
        }
    }

    if let Err(err) = ccb_agent_sidebar::tui::run(args) {
        eprintln!("ccb-agent-sidebar: {err}");
        std::process::exit(1);
    }
}
