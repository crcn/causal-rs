# TODO - Post v0.5.0 Migration

## High Priority

- [ ] Update test suite to new API (77+ tests in engine.rs)
  - Tests use old trait-based API
  - Need to update to closure-based API
  - See API_MIGRATION.md for patterns

## Medium Priority

- [ ] Update outbox crate to new API
  - Remove imports of deleted types (EventBus, Event, CorrelationId)
  - Update to use new Engine/Handle pattern
  - May need to rethink outbox integration

- [ ] Update examples to new API
  - examples/http-fetcher
  - examples/ai-summarizer
  - Both need closure-based API updates

## Low Priority

- [ ] Consider re-adding request/response pattern
  - Old `dispatch_request` was useful
  - Could be reimplemented with pipedream

- [ ] Performance benchmarks
  - Compare old vs new architecture
  - Measure overhead of pipedream vs broadcast

- [ ] Additional documentation
  - More real-world examples
  - Integration patterns
  - Best practices guide

## Notes

The core library (v0.5.0) is **production-ready** and fully functional.
The items above are enhancements that can be done incrementally.
