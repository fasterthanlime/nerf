#[macro_use]
extern crate log;

use std::env;
use std::error::Error;
use std::process::exit;
use structopt::StructOpt;

use nperf_core::{
    args,
    cmd_annotate,
    cmd_collate,
    cmd_csv,
    cmd_metadata,
    cmd_perfetto,
    cmd_trace_events
};

#[cfg(target_os = "macos")]
use nperf_core::{cmd_record_mac, cmd_setup_mac};
#[cfg(not(target_os = "macos"))]
use nperf_core::cmd_record;

#[cfg(feature = "inferno")]
use nperf_core::cmd_flamegraph;

fn main_impl() -> Result< (), Box< dyn Error > > {
    if env::var( "RUST_LOG" ).is_err() {
        env::set_var( "RUST_LOG", "info" );
    }

    #[cfg(feature = "env_logger")]
    env_logger::init();

    let opt = args::Opt::from_args();
    match opt {
        args::Opt::Record( args ) => {
            if args.profiler_args.panic_on_partial_backtrace {
                warn!( "Will panic on partial backtraces!" );
                if env::var( "RUST_BACKTRACE" ).is_err() {
                    env::set_var( "RUST_BACKTRACE", "1" );
                }
            }

            #[cfg(target_os = "macos")]
            {
                if args.serve.is_some() {
                    return Err( "--serve is not yet supported on macOS".into() );
                }
                cmd_record_mac::main( args )?;
            }
            #[cfg(not(target_os = "macos"))]
            run_record_linux( args )?;
        },
        #[cfg(feature = "inferno")]
        args::Opt::Flamegraph( args ) => {
            cmd_flamegraph::main( args )?;
        },
        args::Opt::Csv( args ) => {
            cmd_csv::main( args )?;
        },
        args::Opt::Collate( args ) => {
            cmd_collate::main( args )?;
        },
        args::Opt::Annotate( args ) => {
            cmd_annotate::main( args )?;
        },
        args::Opt::Metadata( args ) => {
            cmd_metadata::main( args )?;
        },
        args::Opt::Perfetto( args ) => {
            cmd_perfetto::main( args )?;
        },
        args::Opt::TraceEvents( args ) => {
            cmd_trace_events::main( args )?;
        },
        #[cfg(target_os = "macos")]
        args::Opt::Setup( args ) => {
            cmd_setup_mac::main( args )?;
        }
    }

    Ok(())
}

fn main() {
    if let Err( error ) = main_impl() {
        eprintln!( "error: {}", error );
        exit( 1 );
    }
}

#[cfg(not(target_os = "macos"))]
fn run_record_linux( args: args::RecordArgs ) -> Result< (), Box< dyn Error > > {
    if let Some( ref addr ) = args.serve {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (sink, _server_handle) = runtime.block_on( nperf_live::start( addr ) )?;
        let result = cmd_record::main_with_live_sink( args, Some( Box::new( sink ) ) );
        // runtime drops here, aborting the server task
        drop( runtime );
        result
    } else {
        cmd_record::main( args )
    }
}
