use logic::{CallbackInput, Coords, GridMap, ObjDetails, ProgramError, Team, GRID_SIZE};
use std::io::{self, Write};
use termcolor::{BufferedStandardStream, Color, ColorSpec, WriteColor};

pub fn display_turn(turn: &CallbackInput) -> io::Result<()> {
    let mut out = BufferedStandardStream::stdout(termcolor::ColorChoice::Auto);

    let mut bold = ColorSpec::new();
    bold.set_bold(true);
    out.set_color(&bold)?;
    writeln!(out, "\nAfter turn {}:", turn.state.turn)?;
    out.reset()?;

    let grid_map = GridMap::from(&turn.state.objs);
    for y in 0..GRID_SIZE {
        let mut first = true;
        let mut prev_wall = false;
        for x in 0..GRID_SIZE {
            let details = grid_map
                .get(&Coords(x, y))
                .map(|id| turn.state.objs.get(&id).unwrap().details());
            let cur_wall = matches!(details, Some(ObjDetails::Terrain(_)));
            if first {
                first = false
            } else if prev_wall && cur_wall {
                write!(out, "█")?;
            } else {
                write!(out, " ")?;
            }
            prev_wall = cur_wall;
            match details {
                Some(ObjDetails::Terrain(_)) => write!(out, "█")?,
                Some(ObjDetails::Unit(unit)) => {
                    let mut spec = ColorSpec::new();
                    spec.set_bg(Some(team_color(unit.team)));
                    // spec.set_fg(Some(Color::White));
                    out.set_color(&spec)?;
                    write!(out, "{}", unit.health)?;
                    out.reset()?;
                }
                None => write!(out, " ")?,
            };
        }
        writeln!(out)?;
    }

    write_turn_info_values(&mut out, turn)?;
    writeln!(out)?;

    for (&team, logs) in &turn.logs {
        if !logs.is_empty() {
            let color = team_color(team);

            let mut header = bold.clone();
            header.set_fg(Some(color));
            out.set_color(&header)?;
            writeln!(out, "Logs for {:?}", team)?;

            let mut bg = ColorSpec::new();
            bg.set_bg(Some(color));
            for log in logs.iter().flat_map(|log| log.trim_end().lines()) {
                out.set_color(&bg)?;
                write!(out, "|{:?}|", team)?;
                out.reset()?;
                writeln!(out, " {}", log)?;
            }
        }
    }

    out.flush()?;
    Ok(())
}

fn team_color(team: Team) -> Color {
    match team {
        Team::Red => Color::Red,
        Team::Blue => Color::Blue,
    }
}

fn compute_turn_info_values(turn_state: &logic::CallbackInput) -> (usize, usize, usize, usize) {
    let objs_list: Vec<&logic::Obj> = turn_state.state.objs.values().collect();

    let robot_filter = |team: logic::Team, obj: &logic::Obj| match &obj.1 {
        logic::ObjDetails::Unit(unit) => unit.team == team,
        _ => false,
    };

    let red_robots: Vec<_> = objs_list
        .iter()
        .filter(|&&robot| robot_filter(logic::Team::Red, robot))
        .cloned()
        .collect();
    let blue_robots: Vec<_> = objs_list
        .iter()
        .filter(|&&robot| robot_filter(logic::Team::Blue, robot))
        .cloned()
        .collect();

    let total_health = |robots: Vec<&logic::Obj>| {
        robots.iter().fold(0, |acc, obj| match &obj.1 {
            logic::ObjDetails::Unit(unit) => acc + unit.health,
            _ => acc,
        })
    };

    (
        red_robots.len(),
        blue_robots.len(),
        total_health(red_robots),
        total_health(blue_robots),
    )
}

pub fn write_turn_info_values(
    out: &mut BufferedStandardStream,
    turn_state: &logic::CallbackInput,
) -> io::Result<()> {
    let (rc, bc, rh, bh) = compute_turn_info_values(turn_state);

    // spec.set_fg(Some(Color::White));
    write!(out, "Health ")?;
    let mut spec = ColorSpec::new();
    spec.set_fg(Some(team_color(Team::Blue)));
    out.set_color(&spec)?;
    write!(out, "{} ", bh)?;
    spec.set_fg(Some(team_color(Team::Red)));
    out.set_color(&spec)?;
    write!(out, "{}", rh)?;
    out.reset()?;
    write!(out, " Units ")?;
    spec.set_fg(Some(team_color(Team::Blue)));
    out.set_color(&spec)?;
    write!(out, "{} ", bc)?;
    spec.set_fg(Some(team_color(Team::Red)));
    out.set_color(&spec)?;
    write!(out, "{}", rc)?;
    out.reset()?;
    out.flush()?;
    Ok(())
}

pub fn display_output(output: logic::MainOutput) -> io::Result<()> {
    if let Some(w) = output.winner {
        println!("Done! {:?} won", w);
    } else {
        println!("Done! it was a tie");
    }

    print!("Final state: ");

    let mut out = BufferedStandardStream::stdout(termcolor::ColorChoice::Auto);
    write_turn_info_values(&mut out, output.turns.last().expect("No final turn!"))?;

    if !output.errors.is_empty() {
        println!("Some errors occurred:");
        for (team, error) in output.errors {
            println!("  {:?}:", team);
            display_error(error)
        }
    }

    Ok(())
}

fn display_error(err: ProgramError) {
    match err {
        ProgramError::InitError(error) => {
            let indent = |s| textwrap::indent(s, "    ");
            println!("{}", indent(&error.summary));
            if let Some(details) = error.details {
                println!("{}", indent(&details));
            }
        }
        _ => println!("    {:?}", err),
    }
}
