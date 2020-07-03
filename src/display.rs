use colored::*;
use logic::{GridMap2D, ObjDetails, State, Team};

pub fn display_state(state: &State) {
    print!("\n");
    for row in GridMap2D::from(state.grid.clone()) {
        for col in row {
            let s = match col {
                Some(id) => {
                    let obj = state.objs.get(&id).unwrap();
                    match &obj.1 {
                        ObjDetails::Terrain(_) => "■".white(),
                        ObjDetails::Unit(unit) => match unit.team {
                            Team::Red => "r".white().on_red(),
                            Team::Blue => "b".white().on_blue(),
                        },
                    }
                }
                None => " ".into(),
            };
            print!("{} ", s);
        }
        print!("\n");
    }
}
