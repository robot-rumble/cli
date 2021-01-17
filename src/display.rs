use logic::{CallbackInput, Coords, GridMap, ObjDetails, Team, GRID_SIZE};
use std::io::{self, Write};
use termcolor::{BufferedStandardStream, Color, ColorSpec, WriteColor};

pub fn display_turn(turn: &CallbackInput) -> io::Result<()> {
    let mut out = BufferedStandardStream::stdout(termcolor::ColorChoice::Auto);

    let mut bold = ColorSpec::new();
    bold.set_bold(true);
    out.set_color(&bold)?;
    writeln!(out, "After turn {}:", turn.state.turn)?;
    out.reset()?;

    let grid_map = GridMap::from(&turn.state.objs);
    for x in 0..GRID_SIZE {
        let mut first = true;
        let mut prev_wall = false;
        for y in 0..GRID_SIZE {
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
