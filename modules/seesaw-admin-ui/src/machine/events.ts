/**
 * Base event type — discriminated union foundation.
 */
export type BaseEvent<TType extends string, TPayload = unknown> = {
  type: TType;
  payload?: TPayload;
};

export type Dispatch<TEvent extends BaseEvent<any, any>> = (
  event: TEvent
) => void;
