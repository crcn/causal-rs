import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";

/**
 * Scrubber playback engine — advances scrubberPosition on a timer
 * when scrubberPlaying is true.
 */
export const createScrubberEngine: EngineCreator<
  InspectorState,
  InspectorMachineEvent
> = (dispatch, getState) => {
  let timer: ReturnType<typeof setInterval> | null = null;

  function stop() {
    if (timer != null) {
      clearInterval(timer);
      timer = null;
    }
  }

  function start() {
    stop();
    const { scrubberSpeed } = getState();
    timer = setInterval(() => {
      const state = getState();
      if (!state.scrubberPlaying || state.flowData.length === 0) {
        stop();
        return;
      }

      const seqs = state.flowData.map((e) => e.seq).sort((a, b) => a - b);
      const currentPos = state.scrubberPosition;

      if (currentPos == null) {
        // Start from the first event
        dispatch({ type: "ui/scrubber_moved", payload: { position: seqs[0] } });
        return;
      }

      // Find next seq after current position
      const nextSeq = seqs.find((s) => s > currentPos);
      if (nextSeq != null) {
        dispatch({ type: "ui/scrubber_moved", payload: { position: nextSeq } });
      } else {
        // Reached the end — stop playing
        dispatch({ type: "ui/scrubber_play_toggled" });
      }
    }, scrubberSpeed);
  }

  return {
    handleEvent: (event, currState) => {
      if (event.type === "ui/scrubber_play_toggled") {
        if (currState.scrubberPlaying) {
          start();
        } else {
          stop();
        }
      }
      if (event.type === "ui/scrubber_speed_changed" && currState.scrubberPlaying) {
        // Restart timer with new speed
        start();
      }
      if (event.type === "ui/flow_opened" || event.type === "ui/flow_closed") {
        stop();
      }
    },
    dispose: () => stop(),
  };
};
