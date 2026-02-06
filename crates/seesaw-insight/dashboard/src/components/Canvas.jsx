import { useState, useEffect } from 'react'
import TreeView from './TreeView'

export default function Canvas({ selectedWorkflow }) {
  const [tree, setTree] = useState(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState(null)

  useEffect(() => {
    if (selectedWorkflow) {
      loadTree(selectedWorkflow)
    }
  }, [selectedWorkflow])

  async function loadTree(correlationId) {
    setLoading(true)
    setError(null)

    try {
      const response = await fetch(`/api/tree/${correlationId}`)
      if (!response.ok) throw new Error('Failed to load tree')
      const data = await response.json()
      setTree(data)
    } catch (err) {
      setError(err.message)
    } finally {
      setLoading(false)
    }
  }

  if (!selectedWorkflow) {
    return (
      <div className="flex-1 bg-slate-950 overflow-auto p-8">
        <div className="max-w-7xl mx-auto h-full flex flex-col items-center justify-center text-center">
          <div className="text-6xl mb-6">🌳</div>
          <h2 className="text-2xl font-semibold text-slate-300 mb-2">Select a workflow to visualize</h2>
          <p className="text-slate-500">Click any event in the live stream to see its workflow tree</p>
        </div>
      </div>
    )
  }

  if (loading) {
    return (
      <div className="flex-1 bg-slate-950 overflow-auto p-8">
        <div className="max-w-7xl mx-auto h-full flex flex-col items-center justify-center">
          <div className="text-6xl mb-6">⏳</div>
          <h2 className="text-2xl font-semibold text-slate-300">Loading workflow...</h2>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex-1 bg-slate-950 overflow-auto p-8">
        <div className="max-w-7xl mx-auto h-full flex flex-col items-center justify-center">
          <div className="text-6xl mb-6">❌</div>
          <h2 className="text-2xl font-semibold text-slate-300 mb-2">Failed to load workflow</h2>
          <p className="text-slate-500">{error}</p>
        </div>
      </div>
    )
  }

  return (
    <div className="flex-1 bg-slate-950 overflow-auto p-8 scrollbar-thin">
      <div className="max-w-7xl mx-auto">
        <TreeView tree={tree} correlationId={selectedWorkflow} />
      </div>
    </div>
  )
}
