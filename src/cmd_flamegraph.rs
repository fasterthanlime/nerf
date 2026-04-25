use std::error::Error;
use std::io;
use std::fs::File;

use inferno::flamegraph;

use crate::args;
use crate::cmd_collate::collapse_into_sorted_vec;

fn inferno_args_into_opts( args: args::ArgsInferno ) -> flamegraph::Options< 'static > {
    let mut options = flamegraph::Options::default();
    if args.inverted {
        options.direction = flamegraph::Direction::Inverted;
    }
    options.reverse_stack_order = args.reverse;
    options.notes = args.notes.unwrap_or_default();
    options.min_width = args.min_width;
    options.image_width = args.image_width;
    if let Some( palette ) = args.palette {
        options.colors = palette;
    }

    options
}

pub fn main( args: args::FlamegraphArgs ) -> Result< (), Box< dyn Error > > {
    let lines = collapse_into_sorted_vec( &args.collation_args, &args.arg_granularity, &args.arg_merge_threads )?;
    let iter = lines.iter().map( |line| line.as_str() );
    let mut options = inferno_args_into_opts( args.args_inferno );

    if let Some( output ) = args.output {
        let fp = io::BufWriter::new( File::create( output )? );
        flamegraph::from_lines( &mut options, iter, fp ).unwrap();
    } else {
        let stdout = io::stdout();
        let stdout = stdout.lock();
        flamegraph::from_lines( &mut options, iter, stdout ).unwrap();
    }

    Ok(())
}
