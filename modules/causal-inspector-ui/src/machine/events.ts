/**
 * Base event type — discriminated union foundation.
 *
 * When no payload type is given, the event has no `payload` property.
 * When a payload type IS given, `payload` is required.
 * This prevents accidentally omitting required payloads at dispatch sites.
 */
export type BaseEvent<TType extends string, TPayload = void> =
  [TPayload] extends [void]
    ? { type: TType }
    : { type: TType; payload: TPayload };

export type Dispatch<TEvent extends { type: string }> = (
  event: TEvent
) => void;
