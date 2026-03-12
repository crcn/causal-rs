import { createClient } from "graphql-ws";
import type { InspectorTransport } from "@causal/inspector-ui";

export function createTransport(graphqlUrl: string, wsUrl: string): InspectorTransport {
  const wsClient = createClient({ url: wsUrl });

  return {
    async query<T>(query: string, variables?: Record<string, unknown>): Promise<T> {
      const res = await fetch(graphqlUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ query, variables }),
      });
      const json = await res.json();
      if (json.errors) {
        throw new Error(json.errors.map((e: { message: string }) => e.message).join(", "));
      }
      return json.data as T;
    },

    subscribe(
      query: string,
      variables: Record<string, unknown>,
      onData: (data: unknown) => void,
      onError?: (error: unknown) => void,
    ): () => void {
      let disposed = false;

      const unsubscribe = wsClient.subscribe(
        { query, variables },
        {
          next(value) {
            if (!disposed && value.data) {
              onData(value.data);
            }
          },
          error(err) {
            if (!disposed) onError?.(err);
          },
          complete() {},
        },
      );

      return () => {
        disposed = true;
        unsubscribe();
      };
    },
  };
}
