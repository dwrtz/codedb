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
    #[command(about = "Emit a deterministic C projection for debugging and inspection")]
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
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    ReplaceBody {
        db: PathBuf,
        name: String,
        expr: String,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    ChangeSignature {
        db: PathBuf,
        name: String,
        signature: String,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    DeleteSymbol {
        db: PathBuf,
        name: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    CreateAlias {
        db: PathBuf,
        name: String,
        alias: String,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    SetExport {
        db: PathBuf,
        name: String,
        exported_name: String,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    RemoveExport {
        db: PathBuf,
        name: String,
        exported_name: String,
        #[arg(long)]
        expect_root: Option<String>,
        #[arg(long)]
        json: bool,
    },
    ExportMap {
        db: PathBuf,
    },
    Diff {
        db: PathBuf,
        root_a: String,
        root_b: String,
        #[arg(long)]
        json: bool,
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
            println!("emitted C projection {}", out.display());
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
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.rename_main_branch_expected_format(
                    &old_name,
                    &new_name,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::ReplaceBody {
            db,
            name,
            expr,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.replace_body_main_branch_expected_format(
                    &name,
                    &expr,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::ChangeSignature {
            db,
            name,
            signature,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.change_signature_main_branch_expected_format(
                    &name,
                    &signature,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::DeleteSymbol {
            db,
            name,
            force,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.delete_symbol_main_branch_expected_format(
                    &name,
                    force,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::CreateAlias {
            db,
            name,
            alias,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.create_alias_main_branch_expected_format(
                    &name,
                    &alias,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::SetExport {
            db,
            name,
            exported_name,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.set_export_main_branch_expected_format(
                    &name,
                    &exported_name,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::RemoveExport {
            db,
            name,
            exported_name,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.remove_export_main_branch_expected_format(
                    &name,
                    &exported_name,
                    expect_root.as_deref(),
                    json
                )?
            );
        }
        Command::ExportMap { db } => {
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.export_map_main_branch()?);
        }
        Command::Diff {
            db,
            root_a,
            root_b,
            json,
        } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.diff_roots_json(&root_a, &root_b)?);
            } else {
                print!("{}", codedb.diff_roots(&root_a, &root_b)?);
            }
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
