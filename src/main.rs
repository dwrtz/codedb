use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "codedb")]
#[command(about = "A content-addressed semantic DAG proof of concept")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        db: PathBuf,
    },
    Import {
        db: PathBuf,
        file: PathBuf,
    },
    Export {
        db: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        out: PathBuf,
    },
    Eval {
        db: PathBuf,
        function_name: String,
        args: Vec<String>,
    },
    EmitC {
        db: PathBuf,
        function_name: String,
        #[arg(long)]
        out: PathBuf,
    },
    List {
        db: PathBuf,
    },
    Show {
        db: PathBuf,
        symbol_or_name: String,
    },
    Callers {
        db: PathBuf,
        symbol_or_name: String,
    },
    Rename {
        db: PathBuf,
        old_name: String,
        new_name: String,
    },
    ReplaceBody {
        db: PathBuf,
        name: String,
        expr: String,
    },
    ChangeSignature {
        db: PathBuf,
        name: String,
        signature: String,
    },
    DeleteSymbol {
        db: PathBuf,
        name: String,
        #[arg(long)]
        force: bool,
    },
    CreateAlias {
        db: PathBuf,
        name: String,
        alias: String,
    },
    Diff {
        db: PathBuf,
        root_a: String,
        root_b: String,
    },
    History {
        db: PathBuf,
    },
    Replay {
        db: PathBuf,
        #[arg(long)]
        from_genesis: bool,
    },
    Verify {
        db: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { db } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let root = codedb.init()?;
            println!("initialized");
            println!("root {root}");
        }
        Command::Import { db, file } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let report = codedb.import_file(&file)?;
            print!("{report}");
        }
        Command::Export { db, branch, out } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let source = codedb.export_branch(&branch)?;
            std::fs::write(&out, source)?;
            println!("exported {}", out.display());
        }
        Command::Eval {
            db,
            function_name,
            args,
        } => {
            let codedb = codedb::CodeDb::open(db)?;
            let parsed_args = args
                .iter()
                .map(|arg| arg.parse::<i64>().map(codedb::Value::I64))
                .collect::<Result<Vec<_>, _>>()?;
            let value = codedb.eval_main_branch(&function_name, parsed_args)?;
            println!("{value}");
        }
        Command::EmitC {
            db,
            function_name,
            out,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let source = codedb.emit_c_main_branch(&function_name)?;
            std::fs::write(&out, source)?;
            println!("emitted {}", out.display());
        }
        Command::List { db } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.list_main_branch()?);
        }
        Command::Show { db, symbol_or_name } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.show_main_branch(&symbol_or_name)?);
        }
        Command::Callers { db, symbol_or_name } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.callers_main_branch(&symbol_or_name)?);
        }
        Command::Rename {
            db,
            old_name,
            new_name,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.rename_main_branch(&old_name, &new_name)?);
        }
        Command::ReplaceBody { db, name, expr } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.replace_body_main_branch(&name, &expr)?);
        }
        Command::ChangeSignature {
            db,
            name,
            signature,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.change_signature_main_branch(&name, &signature)?
            );
        }
        Command::DeleteSymbol { db, name, force } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.delete_symbol_main_branch(&name, force)?);
        }
        Command::CreateAlias { db, name, alias } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.create_alias_main_branch(&name, &alias)?);
        }
        Command::Diff { db, root_a, root_b } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.diff_roots(&root_a, &root_b)?);
        }
        Command::History { db } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.history_main_branch()?);
        }
        Command::Replay { db, from_genesis } => {
            if !from_genesis {
                anyhow::bail!("replay currently requires --from-genesis");
            }
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.replay_main_branch()?);
        }
        Command::Verify { db } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.verify()?);
        }
    }

    Ok(())
}
