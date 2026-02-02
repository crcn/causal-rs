#!/bin/bash

# Script to update tests to new API
# Pattern: Engine::new(state) -> Engine::new() + activate(state)

FILE="crates/seesaw/src/engine.rs"

# Create a backup
cp "$FILE" "${FILE}.backup"

# Replace Engine::new(Counter { value: N }) patterns
# This is complex, so we'll focus on simpler patterns

# Pattern 1: Simple Engine::new(Counter::default())
sed -i '' 's/Engine::new(Counter::default())/Engine::new()/g' "$FILE"
sed -i '' 's/Engine::new(Counter { value: [0-9]* })/Engine::new()/g' "$FILE"

# Pattern 2: store.state() -> handle.context.curr_state()  
sed -i '' 's/store\.state()/handle.context.curr_state()/g' "$FILE"

# Pattern 3: let result = store.activate() -> let result = store.activate(Counter::default())
# This is harder - need context

echo "Basic replacements done. Manual fixes needed for:"
echo "1. activate() calls need state parameter"
echo "2. Test assertions need updating"
echo "3. Some tests may need restructuring"

