import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import { getScrubberSequence } from "../utils";

/**
 * Scrubber playback engine — advances scrubberEnd on a timer
 * when scrubberPlaying is true.
 *
 * State-reactive: watches (curr, prev) diffs instead of specific events.
 * Context-sensitive: derives the walkable sequence from current state
 * (global events, flow events, or filtered subset).
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
      if (!state.scrubberPlaying) {
        stop();
        return;
      }

      const seqs = getScrubberSequence(state);
      if (seqs.length === 0) {
        stop();
        return;
      }

      const currentEnd = state.scrubberEnd;

      if (currentEnd == null) {
        // At "show all" — set to first seq to begin replay
        dispatch({ type: "ui/scrubber_end_changed", payload: { end: seqs[0] } });
        return;
      }

      // Find next seq after current end position
      const nextSeq = seqs.find((s) => s > currentEnd);
      if (nextSeq != null) {
        dispatch({ type: "ui/scrubber_end_changed", payload: { end: nextSeq } });
      } else {
        // Reached the end — stop playing
        dispatch({ type: "ui/scrubber_play_toggled" });
      }
    }, scrubberSpeed);
  }

  return {
    handleEvent: (_event, curr, prev) => {
      // Playing state changed
      if (curr.scrubberPlaying !== prev.scrubberPlaying) {
        if (curr.scrubberPlaying) start();
        else stop();
      }

      // Speed changed while playing → restart with new interval
      if (curr.scrubberSpeed !== prev.scrubberSpeed && curr.scrubberPlaying) {
        start();
      }

      // Flow changed → stop playback
      if (curr.flowCorrelationId !== prev.flowCorrelationId) {
        stop();
      }
    },
    dispose: () => stop(),
  };
};
