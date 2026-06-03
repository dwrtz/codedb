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
    #[command(about = "Apply structural operations from a codedb/apply/v1 JSON file")]
    Apply {
        db: PathBuf,
        #[arg(long)]
        json: PathBuf,
    },
    Export {
        db: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Export a branch migration history as canonical NDJSON")]
    ExportHistory {
        db: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Import a canonical NDJSON migration history into an empty branch")]
    ImportHistory {
        db: PathBuf,
        file: PathBuf,
    },
    Eval {
        db: PathBuf,
        function_name: String,
        args: Vec<String>,
    },
    #[command(about = "Run the reference evaluator and emit a deterministic semantic trace")]
    Trace {
        db: PathBuf,
        entry_name: String,
        args: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Emit a deterministic C projection for debugging and inspection")]
    EmitC {
        db: PathBuf,
        function_name: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Emit target-independent lowered IR for debugging and backend inspection")]
    EmitIr {
        db: PathBuf,
        function_name: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Emit a native relocatable object artifact for one lowered function")]
    EmitObject {
        db: PathBuf,
        function_name: String,
        #[arg(long, default_value = codedb::DEFAULT_NATIVE_TARGET)]
        target: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Emit and cache a deterministic native link plan for an entry function")]
    LinkNative {
        db: PathBuf,
        entry_name: String,
        #[arg(long, default_value = codedb::DEFAULT_NATIVE_TARGET)]
        target: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Emit a deterministic native build plan as JSON")]
    BuildPlan {
        db: PathBuf,
        entry_name: String,
        #[arg(long, default_value = codedb::DEFAULT_NATIVE_TARGET)]
        target: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Build a native executable for an entry function through a cached link plan")]
    Build {
        db: PathBuf,
        entry_name: String,
        #[arg(long, default_value = codedb::DEFAULT_NATIVE_TARGET)]
        target: String,
        #[arg(long)]
        out: PathBuf,
    },
    #[command(about = "Show artifact job and cache status as JSON")]
    ArtifactStatus {
        db: PathBuf,
        #[arg(long)]
        json: bool,
    },
    List {
        db: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Show {
        db: PathBuf,
        symbol_or_name: String,
        #[arg(long)]
        json: bool,
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
    RemoveAlias {
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
        #[arg(long)]
        json: bool,
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
        #[arg(long)]
        json: bool,
    },
    Branches {
        db: PathBuf,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Manage semantic branch pointers")]
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
    },
    Replay {
        db: PathBuf,
        #[arg(long)]
        from_genesis: bool,
    },
    Verify {
        db: PathBuf,
    },
    #[command(about = "Serve the semantic workspace API over local HTTP JSON requests")]
    Serve {
        db: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
    },
}

#[derive(Debug, Subcommand)]
enum BranchCommand {
    Create {
        db: PathBuf,
        name: String,
        #[arg(long = "from")]
        from: Option<String>,
        #[arg(long)]
        from_root: Option<String>,
        #[arg(long)]
        from_history: Option<String>,
        #[arg(long)]
        json: bool,
    },
    List {
        db: PathBuf,
        #[arg(long)]
        json: bool,
    },
    FastForward {
        db: PathBuf,
        target: String,
        source: String,
        #[arg(long)]
        expect_root: String,
        #[arg(long)]
        json: bool,
    },
    Delete {
        db: PathBuf,
        name: String,
        #[arg(long)]
        json: bool,
    },
    Compare {
        db: PathBuf,
        branch_a: String,
        branch_b: String,
        #[arg(long)]
        json: bool,
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
        Command::Apply { db, json } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.apply_json_file(&json)?);
        }
        Command::Export { db, branch, out } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let source = codedb.export_branch(&branch)?;
            std::fs::write(&out, source)?;
            println!("exported {}", out.display());
        }
        Command::ExportHistory { db, branch, out } => {
            let codedb = codedb::CodeDb::open(db)?;
            let history = codedb.export_history_branch(&branch)?;
            std::fs::write(&out, history)?;
            println!("exported history {}", out.display());
        }
        Command::ImportHistory { db, file } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.import_history_file(&file)?);
        }
        Command::Eval {
            db,
            function_name,
            args,
        } => {
            let codedb = codedb::CodeDb::open(db)?;
            let value = codedb.eval_main_branch_text_args(&function_name, &args)?;
            println!("{value}");
        }
        Command::Trace {
            db,
            entry_name,
            args,
            json,
        } => {
            if !json {
                anyhow::bail!("trace currently requires --json");
            }
            let codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.trace_main_branch_text_args_json(&entry_name, &args)?
            );
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
        Command::EmitIr {
            db,
            function_name,
            out,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let ir = codedb.emit_ir_main_branch(&function_name)?;
            std::fs::write(&out, ir)?;
            println!("emitted lowered IR {}", out.display());
        }
        Command::EmitObject {
            db,
            function_name,
            target,
            out,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let object = codedb.emit_object_main_branch(&function_name, &target)?;
            std::fs::write(&out, object)?;
            println!("emitted native object {}", out.display());
        }
        Command::LinkNative {
            db,
            entry_name,
            target,
            out,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let plan = codedb.link_plan_main_branch(&entry_name, &target)?;
            std::fs::write(&out, plan)?;
            println!("emitted native link plan {}", out.display());
        }
        Command::BuildPlan {
            db,
            entry_name,
            target,
            json,
        } => {
            if !json {
                anyhow::bail!("build-plan currently requires --json");
            }
            let mut codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.build_plan_main_branch(&entry_name, &target)?);
        }
        Command::Build {
            db,
            entry_name,
            target,
            out,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            let build = codedb.build_main_branch(&entry_name, &target)?;
            std::fs::write(&out, build.executable)?;
            make_executable(&out)?;
            println!("built native executable {}", out.display());
        }
        Command::ArtifactStatus { db, json } => {
            if !json {
                anyhow::bail!("artifact-status currently requires --json");
            }
            let codedb = codedb::CodeDb::open(db)?;
            print!("{}", codedb.artifact_status_json()?);
        }
        Command::List { db, json } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.list_main_branch_json()?);
            } else {
                print!("{}", codedb.list_main_branch()?);
            }
        }
        Command::Show {
            db,
            symbol_or_name,
            json,
        } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.show_main_branch_json(&symbol_or_name)?);
            } else {
                print!("{}", codedb.show_main_branch(&symbol_or_name)?);
            }
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
        Command::RemoveAlias {
            db,
            name,
            alias,
            expect_root,
            json,
        } => {
            let mut codedb = codedb::CodeDb::open(db)?;
            print!(
                "{}",
                codedb.remove_alias_main_branch_expected_format(
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
        Command::ExportMap { db, json } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.export_map_main_branch_json()?);
            } else {
                print!("{}", codedb.export_map_main_branch()?);
            }
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
        Command::History { db, json } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.history_main_branch_json()?);
            } else {
                print!("{}", codedb.history_main_branch()?);
            }
        }
        Command::Branches { db, json } => {
            let codedb = codedb::CodeDb::open(db)?;
            if json {
                print!("{}", codedb.branches_json()?);
            } else {
                print!("{}", codedb.branches()?);
            }
        }
        Command::Branch { command } => match command {
            BranchCommand::Create {
                db,
                name,
                from,
                from_root,
                from_history,
                json,
            } => {
                let mut codedb = codedb::CodeDb::open(db)?;
                print!(
                    "{}",
                    codedb.create_branch_from(
                        &name,
                        from.as_deref(),
                        from_root.as_deref(),
                        from_history.as_deref(),
                        json
                    )?
                );
            }
            BranchCommand::List { db, json } => {
                let codedb = codedb::CodeDb::open(db)?;
                if json {
                    print!("{}", codedb.branches_json()?);
                } else {
                    print!("{}", codedb.branches()?);
                }
            }
            BranchCommand::FastForward {
                db,
                target,
                source,
                expect_root,
                json,
            } => {
                let mut codedb = codedb::CodeDb::open(db)?;
                print!(
                    "{}",
                    codedb.fast_forward_branch(&target, &source, &expect_root, json)?
                );
            }
            BranchCommand::Delete { db, name, json } => {
                let mut codedb = codedb::CodeDb::open(db)?;
                print!("{}", codedb.delete_branch(&name, json)?);
            }
            BranchCommand::Compare {
                db,
                branch_a,
                branch_b,
                json,
            } => {
                let codedb = codedb::CodeDb::open(db)?;
                print!("{}", codedb.compare_branches(&branch_a, &branch_b, json)?);
            }
        },
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
        Command::Serve { db, addr } => {
            codedb::server::serve_workspace(db, &addr)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
