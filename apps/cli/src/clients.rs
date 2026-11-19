//! The commands that a client and a schedule run: `register`, `release`,
//! `pressure`, `maintain`, and `schedule`.

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use suds_core::development::Environment;
use suds_core::maintain::{self, Context, MaintenanceReport};
use suds_core::pressure::{Level, Pressure};
use suds_core::reclaim::git::Git;
use suds_core::registry::{self, Event, Record};
use suds_core::roots;
use suds_core::state::{Directories, load_settings};
use suds_core::stores::Programs;

use crate::cli::{Format, HoldArgs, MaintainArgs, PressureArgs, ScheduleArgs, Scheduler};
use crate::now_seconds;
use crate::output::{emit, finish, planned, ran};
use crate::table::format_bytes;

fn directories() -> Result<Directories, String> {
    Directories::of(&Environment::of_process())
        .ok_or_else(|| "no home directory exists for this process".to_owned())
}

pub fn register_command(args: &HoldArgs) -> ExitCode {
    finish(hold(args, Event::Registered))
}

pub fn release_command(args: &HoldArgs) -> ExitCode {
    finish(hold(args, Event::Released))
}

/// Appends one record to the registry. The checkout's path resolves against
/// the working directory, without following a link.
fn hold(args: &HoldArgs, event: Event) -> Result<ExitCode, String> {
    let cwd =
        env::current_dir().map_err(|err| format!("could not read the working directory: {err}"))?;
    let checkout = roots::normalize(&cwd.join(&args.checkout));
    let record = registry::append(
        &directories()?.registry(),
        &checkout,
        &args.client,
        args.attempt.as_deref(),
        event,
        now_seconds(),
    )?;
    planned(emit(Format::Json, &record, |_, _: &Record| Ok(())))
}

pub fn pressure_command(args: &PressureArgs) -> ExitCode {
    finish(pressure(args))
}

/// Prints the last reading that a maintenance run recorded. The command
/// measures nothing, so a client can read the level before each start.
fn pressure(args: &PressureArgs) -> Result<ExitCode, String> {
    let path = directories()?.pressure();
    let reading = maintain::read_pressure(&path).ok_or_else(|| {
        format!(
            "{} holds no reading. Run suds maintain to measure.",
            path.display()
        )
    })?;
    planned(emit(args.format, &reading, write_pressure))
}

fn write_pressure(out: &mut impl Write, reading: &Pressure) -> io::Result<()> {
    let level = match reading.level {
        Level::Normal => "normal",
        Level::Soft => "soft",
        Level::Hard => "hard",
    };
    let limit = if reading.enforced {
        "a filesystem limit enforces the ceiling"
    } else {
        "no filesystem limit enforces the ceiling"
    };
    writeln!(
        out,
        "{level}: {} of {} at {}, and {limit}",
        format_bytes(reading.usage_bytes),
        format_bytes(reading.ceiling_bytes),
        reading.observed_at
    )
}

pub fn maintain_command(args: &MaintainArgs) -> ExitCode {
    finish(run_maintenance(args))
}

fn run_maintenance(args: &MaintainArgs) -> Result<ExitCode, String> {
    let directories = directories()?;
    let settings = load_settings(&directories.settings())?;
    let context = Context {
        directories: &directories,
        environment: &Environment::of_process(),
        programs: &Programs::default(),
        git: &Git::default(),
        now: now_seconds(),
    };
    let report = maintain::maintain(&settings, &context)?;
    let partial = report
        .reclaim
        .iter()
        .flat_map(|reclaim| &reclaim.removals)
        .any(|removal| !removal.removed)
        || report
            .stores
            .iter()
            .flat_map(|stores| &stores.trims)
            .any(|trim| trim.failure.is_some());
    ran(emit(args.format, &report, write_maintenance), partial)
}

fn write_maintenance(out: &mut impl Write, report: &MaintenanceReport) -> io::Result<()> {
    write_pressure(out, &report.pressure)?;
    if !report.standing_approval {
        return writeln!(
            out,
            "The settings hold no standing approval, so Suds measured and changed nothing."
        );
    }
    let reclaimed = report.reclaim.as_ref().map_or(0, |run| run.freed_bytes);
    let trimmed = report.stores.as_ref().map_or(0, |run| run.freed_bytes);
    writeln!(
        out,
        "Suds removed about {} of build state and trimmed about {} from the stores. The journal records each change.",
        format_bytes(reclaimed),
        format_bytes(trimmed)
    )
}

pub fn schedule_command(args: &ScheduleArgs) -> ExitCode {
    finish(schedule(args))
}

/// Prints the units that run `suds maintain` on a schedule. Suds installs
/// nothing, and a person or an installer places the units.
fn schedule(args: &ScheduleArgs) -> Result<ExitCode, String> {
    let program = env::current_exe()
        .map_err(|err| format!("could not find the path of this program: {err}"))?;
    let text = match args.scheduler {
        Scheduler::Systemd => systemd_units(&program, args.minutes),
        Scheduler::Launchd => launchd_agent(&program, args.minutes),
    };
    let mut out = io::stdout().lock();
    planned(out.write_all(text.as_bytes()).and_then(|()| out.flush()))
}

fn systemd_units(program: &Path, minutes: u64) -> String {
    format!(
        "# ~/.config/systemd/user/suds-maintain.service\n\
         [Unit]\n\
         Description=Suds measures and reclaims development storage\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={program} maintain\n\
         \n\
         # ~/.config/systemd/user/suds-maintain.timer\n\
         [Unit]\n\
         Description=Suds maintenance every {minutes} minutes\n\
         \n\
         [Timer]\n\
         OnBootSec=5min\n\
         OnUnitActiveSec={minutes}min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        program = program.display()
    )
}

fn launchd_agent(program: &Path, minutes: u64) -> String {
    let program = xml_escape(&program.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!-- ~/Library/LaunchAgents/sh.pithecene.suds.maintain.plist -->\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>sh.pithecene.suds.maintain</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{program}</string>\n\
         \t\t<string>maintain</string>\n\
         \t</array>\n\
         \t<key>StartInterval</key>\n\
         \t<integer>{seconds}</integer>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        seconds = minutes.saturating_mul(60)
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{launchd_agent, systemd_units};

    #[test]
    fn the_units_run_the_maintenance_of_this_program() {
        let program = Path::new("/opt/suds & co/suds");
        let units = systemd_units(program, 30);
        assert!(units.contains("ExecStart=/opt/suds & co/suds maintain\n"));
        assert!(units.contains("OnUnitActiveSec=30min\n"));
        let agent = launchd_agent(program, 30);
        assert!(agent.contains("<string>/opt/suds &amp; co/suds</string>"));
        assert!(agent.contains("<integer>1800</integer>"));
    }
}
