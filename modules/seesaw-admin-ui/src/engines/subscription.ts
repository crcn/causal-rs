import type { EngineCreator } from "../machine";
import type { AdminMachineEvent } from "../events";
import type { AdminState } from "../state";
import type { AdminEvent } from "../types";
import { EVENTS_SUBSCRIPTION } from "../queries";

export type SubscriptionTransport = {
  /** Subscribe to a GraphQL subscription. Returns an unsubscribe function. */
  subscribe: (
    query: string,
    variables: Record<string, unknown>,
    onData: (data: unknown) => void,
    onError?: (error: unknown) => void
  ) => () => void;
};

/**
 * Subscription engine — connects to the live event stream via WebSocket.
 *
 * On creation, subscribes to `adminEventAdded`. Each event dispatches
 * directly into the reducer as `events/received`.
 *
 * Transport is injected so consumers can use any GraphQL WS client
 * (graphql-ws, Apollo, urql, etc).
 */
export const createSubscriptionEngine = (
  transport: SubscriptionTransport
): EngineCreator<AdminState, AdminMachineEvent> => {
  return (dispatch, getState) => {
    let unsub: (() => void) | null = null;

    const connect = () => {
      const state = getState();
      const lastSeq =
        state.events.length > 0 ? state.events[0].seq : undefined;
      let connected = false;

      unsub = transport.subscribe(
        EVENTS_SUBSCRIPTION,
        lastSeq != null ? { lastSeq } : {},
        (data: unknown) => {
          if (!connected) {
            connected = true;
            dispatch({ type: "events/subscription_connected" });
          }
          const event = (data as { adminEventAdded: AdminEvent })
            .adminEventAdded;
          if (event) {
            dispatch({ type: "events/received", payload: [event] });
          }
        },
        (error) => {
          console.error("[seesaw-admin] subscription error:", error);
          dispatch({
            type: "events/subscription_error",
            payload: { message: String(error) },
          });
        }
      );
    };

    // Connect immediately
    connect();

    return {
      dispose: () => {
        unsub?.();
      },
    };
  };
};
