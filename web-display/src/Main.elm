port module Main exposing (main, getOutput, getProgress, startEval)

import Browser
import Api
import BattleViewer exposing (..)
import Json.Decode as Decode
import OpponentSelect
import Data

type alias Flags =
    { user : String
    , robot : String
    }

decodeOrInternalErr decodeF msg =
    \val ->
        case decodeF val of
            Ok a -> msg a
            Err _ -> GotInternalError

paths : Api.Paths
paths =
    { getRobotCode = "/getrobotcode"
    -- we don't use this, so w/e
    , getUserRobots = "/getrobots"
    , updateRobotCode = "/updaterobot"
    }

main : Program Flags Model Msg
main =
    Browser.element
        { init = \{user, robot} -> init (Api.Context user robot 0 paths) "/"
        , view = view
        , update = \msg old ->
            let
                ( model, renderCmd ) = update msg old
                cmd = case msg of
                    Run turns ->
                        let
                            id = case model.opponentSelectState.opponent of
                                OpponentSelect.Robot ( robot, _ ) -> robot.id
                                OpponentSelect.Itself -> 0
                        in startEval { id = id, turns = turns }
                    _ -> Cmd.none
            in ( model, Cmd.batch [ renderCmd, cmd ] )
            
        , subscriptions = \_ -> Sub.batch
            [ getProgress <| decodeOrInternalErr Data.decodeProgressData GotProgress
            , getOutput <| decodeOrInternalErr Data.decodeOutcomeData GotOutput
            ]
        }

port getOutput : (Decode.Value -> msg) -> Sub msg

port getProgress : (Decode.Value -> msg) -> Sub msg

port startEval : { id : Int, turns : Int } -> Cmd msg

