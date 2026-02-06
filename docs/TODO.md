# TODO - Post v0.5.0 Migration

## Completed ✅

- [x] Update test suite to new API (163 tests passing)
  - All tests migrated to closure-based API
  - See API_MIGRATION.md for patterns

- [x] Update outbox crate to new API
  - Removed imports of deleted types (EventBus, Event)
  - Created standalone CorrelationId within outbox crate
  - Updated to use EmitCallback pattern
  - All tests passing

- [x] Update examples to new API
  - examples/simple-order (new example demonstrating API)
  - examples/http-fetcher (updated to closure-based)
  - examples/ai-summarizer (updated to closure-based)
  - All examples compile and run correctly

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
All critical migration tasks are complete. The items above are enhancements
that can be done incrementally.
