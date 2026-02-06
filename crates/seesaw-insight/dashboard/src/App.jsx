import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import './App.css'

const MAX_ENTRIES = 1200
const STATS_REFRESH_MS = 5000
const OPERATIONS_REFRESH_MS = 4000
const PIN_STORAGE_KEY = 'seesaw-insight:pinned-workflows'

const NODE_WIDTH = 236
const NODE_HEIGHT = 120
const LEVEL_GAP = 312
const ROW_GAP = 146

const UUID_REGEX =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i

function isUuid(value) {
  return typeof value === 'string' && UUID_REGEX.test(value)
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value))
}

function shortId(value) {
  if (!value || typeof value !== 'string') return '—'
  return `${value.slice(0, 8)}…${value.slice(-4)}`
}

function simplifyType(value) {
  if (!value || typeof value !== 'string') return 'Event'
  return value.includes('::') ? value.split('::').pop() : value
}

function formatTimestamp(value) {
  if (!value) return '—'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '—'
  return new Intl.DateTimeFormat(undefined, {
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  }).format(date)
}

function safeLocale(value) {
  if (typeof value !== 'number') return '0'
  return value.toLocaleString()
}

function parseEnvelope(entry) {
  if (
    entry?.stream_type === 'event_dispatched' &&
    entry?.payload &&
    typeof entry.payload === 'object' &&
    !Array.isArray(entry.payload)
  ) {
    return {
      eventType:
        typeof entry.payload.event_type === 'string'
          ? entry.payload.event_type
          : entry.event_type ?? null,
      payload: entry.payload.payload ?? null,
      hops: Number.isFinite(entry.payload.hops) ? Number(entry.payload.hops) : null,
      batchId: typeof entry.payload.batch_id === 'string' ? entry.payload.batch_id : null,
      batchIndex: Number.isFinite(entry.payload.batch_index)
        ? Number(entry.payload.batch_index)
        : null,
      batchSize: Number.isFinite(entry.payload.batch_size)
        ? Number(entry.payload.batch_size)
        : null,
    }
  }

  return {
    eventType: entry?.event_type ?? null,
    payload: entry?.payload ?? null,
    hops: null,
    batchId: null,
    batchIndex: null,
    batchSize: null,
  }
}

function batchProgressLabel(batchIndex, batchSize) {
  if (!Number.isFinite(batchIndex) || !Number.isFinite(batchSize) || batchSize <= 0) return null
  return `batch ${batchIndex + 1}/${batchSize}`
}

function isJoinWaitingResult(result) {
  return (
    result &&
    typeof result === 'object' &&
    !Array.isArray(result) &&
    result.status === 'join_waiting'
  )
}

function previewPayload(value) {
  if (value === null || value === undefined) return ''
  const raw = typeof value === 'string' ? value : JSON.stringify(value)
  return raw.length > 108 ? `${raw.slice(0, 108)}…` : raw
}

function jsonPretty(value) {
  if (value === null || value === undefined) return ''
  return JSON.stringify(value, null, 2)
}

function swallowError(error) {
  void error
}

function readPinned() {
  try {
    const raw = window.localStorage.getItem(PIN_STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed.filter((id) => typeof id === 'string' && isUuid(id))
  } catch (error) {
    swallowError(error)
    return []
  }
}

function streamLabel(streamType) {
  if (streamType === 'event_dispatched') return 'Event'
  if (streamType === 'effect_started') return 'Effect Started'
  if (streamType === 'effect_completed') return 'Effect Completed'
  if (streamType === 'effect_failed') return 'Effect Failed'
  return 'Update'
}

function effectTone(status) {
  if (status === 'failed') return 'tone-fail'
  if (status === 'completed') return 'tone-ok'
  if (status === 'executing') return 'tone-warn'
  return 'tone-neutral'
}

function workflowFailedCount(workflow) {
  return Math.max(workflow?.failed?.failed_effects || 0, workflow?.effect_failures || 0)
}

function workflowHasFailure(workflow) {
  return workflowFailedCount(workflow) > 0
}

function workflowHasDeadLetters(workflow) {
  return (workflow?.dead_letters || 0) > 0
}

function percentile(sortedValues, ratio) {
  if (sortedValues.length === 0) return null
  const index = Math.min(sortedValues.length - 1, Math.floor(sortedValues.length * ratio))
  return sortedValues[index]
}

function buildCanvasGraph(tree) {
  if (!tree || !Array.isArray(tree.roots) || tree.roots.length === 0) {
    return {
      nodes: [],
      edges: [],
      nodeMap: new Map(),
      width: 1200,
      height: 720,
    }
  }

  let leafIndex = 0
  let maxDepth = 0

  const nodes = []
  const edges = []

  const walk = (node, depth, parentId) => {
    maxDepth = Math.max(maxDepth, depth)

    const children = Array.isArray(node?.children) ? node.children : []
    const childYs = []

    for (const child of children) {
      const y = walk(child, depth + 1, node.event_id)
      childYs.push(y)
      edges.push({
        id: `${node.event_id}->${child.event_id}`,
        source: node.event_id,
        target: child.event_id,
      })
    }

    let y
    if (childYs.length === 0) {
      y = leafIndex * ROW_GAP
      leafIndex += 1
    } else {
      y = childYs.reduce((sum, value) => sum + value, 0) / childYs.length
    }

    nodes.push({
      id: node.event_id,
      parent_id: parentId ?? null,
      depth,
      x: depth * LEVEL_GAP,
      y,
      raw: node,
    })

    return y
  }

  for (const root of tree.roots) {
    walk(root, 0, null)
    leafIndex += 0.65
  }

  const maxY = Math.max(...nodes.map((node) => node.y), 0)

  const width = Math.max(1180, (maxDepth + 1) * LEVEL_GAP + NODE_WIDTH + 220)
  const height = Math.max(720, maxY + NODE_HEIGHT + 180)

  const nodeMap = new Map(nodes.map((node) => [node.id, node]))

  return { nodes, edges, nodeMap, width, height }
}

function edgePath(source, target) {
  const sourceX = source.x + NODE_WIDTH
  const sourceY = source.y + NODE_HEIGHT / 2
  const targetX = target.x
  const targetY = target.y + NODE_HEIGHT / 2

  const control = Math.max(56, (targetX - sourceX) * 0.44)

  return `M ${sourceX} ${sourceY} C ${sourceX + control} ${sourceY} ${targetX - control} ${targetY} ${targetX} ${targetY}`
}

function App() {
  const [entries, setEntries] = useState([])
  const [stats, setStats] = useState({
    total_events: 0,
    active_effects: 0,
    completed_effects: 0,
    failed_effects: 0,
  })

  const [deadLetters, setDeadLetters] = useState([])
  const [failedWorkflows, setFailedWorkflows] = useState([])
  const [effectLogs, setEffectLogs] = useState([])

  const [connected, setConnected] = useState(false)
  const [workflowQuery, setWorkflowQuery] = useState('')
  const [workflowView, setWorkflowView] = useState('all')
  const [selectedWorkflowId, setSelectedWorkflowId] = useState(null)
  const [selectedNodeId, setSelectedNodeId] = useState(null)
  const [tree, setTree] = useState(null)
  const [treeLoading, setTreeLoading] = useState(false)
  const [treeError, setTreeError] = useState(null)

  const [pinnedWorkflowIds, setPinnedWorkflowIds] = useState(() => readPinned())
  const [viewport, setViewport] = useState({ x: 44, y: 52, zoom: 1 })
  const [isPanning, setIsPanning] = useState(false)

  const reconnectRef = useRef(null)
  const wsRef = useRef(null)
  const loadedTreeWorkflowRef = useRef(null)
  const fittedWorkflowRef = useRef(null)
  const canvasViewportRef = useRef(null)
  const panRef = useRef({
    active: false,
    pointerId: null,
    startX: 0,
    startY: 0,
    originX: 0,
    originY: 0,
  })

  useEffect(() => {
    try {
      window.localStorage.setItem(PIN_STORAGE_KEY, JSON.stringify(pinnedWorkflowIds))
    } catch (error) {
      swallowError(error)
    }
  }, [pinnedWorkflowIds])

  useEffect(() => {
    let isClosed = false

    const connect = () => {
      if (isClosed) return

      const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
      const ws = new WebSocket(`${protocol}//${window.location.host}/api/ws`)
      wsRef.current = ws

      ws.onopen = () => setConnected(true)
      ws.onerror = () => ws.close()
      ws.onclose = () => {
        setConnected(false)
        if (!isClosed) reconnectRef.current = setTimeout(connect, 1000)
      }

      ws.onmessage = (message) => {
        try {
          const event = JSON.parse(message.data)
          if (event?.type === 'connected' || typeof event?.seq !== 'number') return

          setEntries((previous) => {
            if (previous.some((entry) => entry.seq === event.seq)) return previous
            const next = [event, ...previous]
            return next.slice(0, MAX_ENTRIES)
          })

          if (isUuid(event.correlation_id)) {
            setSelectedWorkflowId((current) => current ?? event.correlation_id)
          }
        } catch (error) {
          swallowError(error)
        }
      }
    }

    connect()

    return () => {
      isClosed = true
      setConnected(false)
      if (reconnectRef.current) clearTimeout(reconnectRef.current)
      if (wsRef.current) wsRef.current.close()
    }
  }, [])

  useEffect(() => {
    let cancelled = false

    const loadStats = async () => {
      try {
        const response = await fetch('/api/stats')
        if (!response.ok) return
        const payload = await response.json()
        if (!cancelled) setStats(payload)
      } catch (error) {
        swallowError(error)
      }
    }

    loadStats()
    const interval = setInterval(loadStats, STATS_REFRESH_MS)

    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [])

  useEffect(() => {
    let cancelled = false

    const loadOps = async () => {
      try {
        const [deadRes, failedRes] = await Promise.all([
          fetch('/api/dead-letters?unresolved_only=true&limit=300'),
          fetch('/api/failures?limit=300'),
        ])

        if (!cancelled && deadRes.ok) {
          setDeadLetters(await deadRes.json())
        }

        if (!cancelled && failedRes.ok) {
          setFailedWorkflows(await failedRes.json())
        }
      } catch (error) {
        swallowError(error)
      }
    }

    loadOps()
    const interval = setInterval(loadOps, OPERATIONS_REFRESH_MS)

    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [])

  const workflows = useMemo(() => {
    const byId = new Map()

    for (const entry of entries) {
      if (!isUuid(entry.correlation_id)) continue

      const current = byId.get(entry.correlation_id) || {
        correlation_id: entry.correlation_id,
        event_count: 0,
        latest_seq: entry.seq,
        latest_at: entry.created_at,
        root: null,
        effect_failures: 0,
      }

      current.event_count += 1

      if (entry.seq > current.latest_seq) {
        current.latest_seq = entry.seq
        current.latest_at = entry.created_at
      }

      if (entry.stream_type === 'effect_failed') {
        current.effect_failures += 1
      }

      if (entry.stream_type === 'event_dispatched') {
        const envelope = parseEnvelope(entry)

        const rootCandidate = {
          seq: entry.seq,
          type: simplifyType(envelope.eventType),
          payload: envelope.payload,
          hops: envelope.hops,
          batchId: envelope.batchId,
          batchIndex: envelope.batchIndex,
          batchSize: envelope.batchSize,
          created_at: entry.created_at,
        }

        const shouldReplaceRoot =
          !current.root ||
          entry.seq < current.root.seq ||
          (rootCandidate.hops === 0 && current.root.hops !== 0)

        if (shouldReplaceRoot) current.root = rootCandidate
      }

      byId.set(entry.correlation_id, current)
    }

    const failureByWorkflow = new Map(
      failedWorkflows.map((workflow) => [workflow.correlation_id, workflow]),
    )

    const deadByWorkflow = new Map()
    for (const row of deadLetters) {
      deadByWorkflow.set(row.correlation_id, (deadByWorkflow.get(row.correlation_id) || 0) + 1)
    }

    const workflowIds = new Set([...byId.keys(), ...failureByWorkflow.keys()])
    const combined = []

    for (const correlationId of workflowIds) {
      const base = byId.get(correlationId) || {
        correlation_id: correlationId,
        event_count: 0,
        latest_seq: -1,
        latest_at: null,
        root: null,
        effect_failures: 0,
      }

      const failed = failureByWorkflow.get(correlationId) || null

      combined.push({
        ...base,
        failed,
        dead_letters: deadByWorkflow.get(correlationId) || failed?.dead_letters || 0,
      })
    }

    return combined
  }, [entries, deadLetters, failedWorkflows])

  const searchMatchedWorkflows = useMemo(() => {
    const query = workflowQuery.trim().toLowerCase()
    if (!query) return workflows

    return workflows.filter((workflow) => {
      const haystack = [
        workflow.correlation_id,
        workflow.root?.type,
        previewPayload(workflow.root?.payload),
        workflow.failed?.last_error,
      ]
        .filter(Boolean)
        .join(' ')
        .toLowerCase()

      return haystack.includes(query)
    })
  }, [workflowQuery, workflows])

  const workflowFilterCounts = useMemo(() => {
    const failed = searchMatchedWorkflows.filter(workflowHasFailure).length
    const dead = searchMatchedWorkflows.filter(workflowHasDeadLetters).length

    return {
      all: searchMatchedWorkflows.length,
      failed,
      dead,
    }
  }, [searchMatchedWorkflows])

  const visibleWorkflows = useMemo(() => {
    if (workflowView === 'failed') {
      return searchMatchedWorkflows.filter(workflowHasFailure)
    }

    if (workflowView === 'dead') {
      return searchMatchedWorkflows.filter(workflowHasDeadLetters)
    }

    return searchMatchedWorkflows
  }, [searchMatchedWorkflows, workflowView])

  const sortedWorkflows = useMemo(() => {
    const pinnedSet = new Set(pinnedWorkflowIds)
    const next = [...visibleWorkflows]

    next.sort((a, b) => {
      const aPinned = pinnedSet.has(a.correlation_id)
      const bPinned = pinnedSet.has(b.correlation_id)

      if (aPinned !== bPinned) return aPinned ? -1 : 1
      return b.latest_seq - a.latest_seq
    })

    return next
  }, [visibleWorkflows, pinnedWorkflowIds])

  useEffect(() => {
    if (!selectedWorkflowId && sortedWorkflows.length > 0) {
      setSelectedWorkflowId(sortedWorkflows[0].correlation_id)
      return
    }

    if (
      selectedWorkflowId &&
      !sortedWorkflows.some((workflow) => workflow.correlation_id === selectedWorkflowId)
    ) {
      setSelectedWorkflowId(sortedWorkflows[0]?.correlation_id ?? null)
    }
  }, [selectedWorkflowId, sortedWorkflows])

  const selectedWorkflow = useMemo(
    () => sortedWorkflows.find((workflow) => workflow.correlation_id === selectedWorkflowId) || null,
    [selectedWorkflowId, sortedWorkflows],
  )

  useEffect(() => {
    if (!selectedWorkflowId || !isUuid(selectedWorkflowId)) {
      setTree(null)
      setTreeError(null)
      loadedTreeWorkflowRef.current = null
      return
    }

    let cancelled = false
    const isSwitch = loadedTreeWorkflowRef.current !== selectedWorkflowId

    if (isSwitch) setTreeLoading(true)
    setTreeError(null)

    const loadTree = async () => {
      try {
        const response = await fetch(`/api/tree/${selectedWorkflowId}`)
        if (!response.ok) throw new Error(`tree request failed (${response.status})`)

        const payload = await response.json()
        if (!cancelled) {
          setTree(payload)
          loadedTreeWorkflowRef.current = selectedWorkflowId
        }
      } catch (error) {
        if (!cancelled) setTreeError(error?.message || 'Failed to load tree')
      } finally {
        if (!cancelled && isSwitch) setTreeLoading(false)
      }
    }

    loadTree()
    const interval = setInterval(loadTree, OPERATIONS_REFRESH_MS)

    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [selectedWorkflowId])

  useEffect(() => {
    if (!selectedWorkflowId || !isUuid(selectedWorkflowId)) {
      setEffectLogs([])
      return
    }

    let cancelled = false

    const loadEffects = async () => {
      try {
        const response = await fetch(
          `/api/effects?correlation_id=${selectedWorkflowId}&limit=300`,
        )
        if (!response.ok) return

        const payload = await response.json()
        if (!cancelled) setEffectLogs(payload)
      } catch (error) {
        swallowError(error)
      }
    }

    loadEffects()
    const interval = setInterval(loadEffects, OPERATIONS_REFRESH_MS)

    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [selectedWorkflowId])

  const selectedWorkflowDeadLetters = useMemo(() => {
    if (!selectedWorkflowId) return []
    return deadLetters.filter((entry) => entry.correlation_id === selectedWorkflowId)
  }, [deadLetters, selectedWorkflowId])

  const selectedWorkflowBatchStats = useMemo(() => {
    if (!tree || !Array.isArray(tree.roots) || tree.roots.length === 0) {
      return {
        batches: 0,
        items: 0,
        joinWaiting: 0,
      }
    }

    const batchIds = new Set()
    let itemCount = 0
    let joinWaiting = 0

    const stack = [...tree.roots]
    while (stack.length > 0) {
      const node = stack.pop()
      if (!node || typeof node !== 'object') continue

      if (typeof node.batch_id === 'string') {
        batchIds.add(node.batch_id)
        itemCount += 1
      }

      const effects = Array.isArray(node.effects) ? node.effects : []
      for (const effect of effects) {
        if (isJoinWaitingResult(effect?.result)) {
          joinWaiting += 1
        }
      }

      const children = Array.isArray(node.children) ? node.children : []
      for (const child of children) {
        stack.push(child)
      }
    }

    return {
      batches: batchIds.size,
      items: itemCount,
      joinWaiting,
    }
  }, [tree])

  const timingStats = useMemo(() => {
    const values = effectLogs
      .map((entry) => entry.duration_ms)
      .filter((value) => typeof value === 'number' && Number.isFinite(value) && value >= 0)
      .sort((a, b) => a - b)

    if (values.length === 0) {
      return { avg: null, p95: null, max: null }
    }

    const sum = values.reduce((acc, value) => acc + value, 0)

    return {
      avg: Math.round(sum / values.length),
      p95: percentile(values, 0.95),
      max: values[values.length - 1],
    }
  }, [effectLogs])

  const graph = useMemo(() => buildCanvasGraph(tree), [tree])

  useEffect(() => {
    if (graph.nodes.length === 0) {
      setSelectedNodeId(null)
      return
    }

    if (!selectedNodeId || !graph.nodeMap.has(selectedNodeId)) {
      setSelectedNodeId(graph.nodes[0].id)
    }
  }, [graph, selectedNodeId])

  const selectedNode = useMemo(() => {
    if (!selectedNodeId) return null
    return graph.nodeMap.get(selectedNodeId) || null
  }, [graph, selectedNodeId])

  const selectedNodeEffects = useMemo(() => {
    if (!selectedNode?.raw?.effects) return []
    return Array.isArray(selectedNode.raw.effects) ? selectedNode.raw.effects : []
  }, [selectedNode])

  const selectedNodeChildren = Array.isArray(selectedNode?.raw?.children)
    ? selectedNode.raw.children.length
    : 0
  const selectedNodeBatchLabel = batchProgressLabel(
    selectedNode?.raw?.batch_index,
    selectedNode?.raw?.batch_size,
  )

  const togglePinned = (workflowId) => {
    setPinnedWorkflowIds((current) => {
      if (current.includes(workflowId)) {
        return current.filter((id) => id !== workflowId)
      }

      return [workflowId, ...current]
    })
  }

  const fitCanvas = useCallback(() => {
    const viewportElement = canvasViewportRef.current
    if (!viewportElement) return

    if (graph.nodes.length === 0) {
      setViewport({ x: 44, y: 52, zoom: 1 })
      return
    }

    const padding = 48
    const zoomX = (viewportElement.clientWidth - padding * 2) / graph.width
    const zoomY = (viewportElement.clientHeight - padding * 2) / graph.height
    const zoom = clamp(Math.min(zoomX, zoomY, 1.2), 0.35, 2.2)

    setViewport({
      zoom,
      x: (viewportElement.clientWidth - graph.width * zoom) / 2,
      y: (viewportElement.clientHeight - graph.height * zoom) / 2,
    })
  }, [graph.height, graph.nodes.length, graph.width])

  useEffect(() => {
    fittedWorkflowRef.current = null
  }, [selectedWorkflowId])

  useEffect(() => {
    if (!selectedWorkflowId || graph.nodes.length === 0) return
    if (fittedWorkflowRef.current === selectedWorkflowId) return

    fitCanvas()
    fittedWorkflowRef.current = selectedWorkflowId
  }, [fitCanvas, graph.nodes.length, selectedWorkflowId])

  const adjustZoom = useCallback((delta) => {
    const viewportElement = canvasViewportRef.current

    setViewport((current) => {
      const nextZoom = clamp(current.zoom + delta, 0.35, 2.2)

      if (!viewportElement) {
        return { ...current, zoom: nextZoom }
      }

      const centerX = viewportElement.clientWidth / 2
      const centerY = viewportElement.clientHeight / 2
      const ratio = nextZoom / current.zoom

      return {
        zoom: nextZoom,
        x: centerX - (centerX - current.x) * ratio,
        y: centerY - (centerY - current.y) * ratio,
      }
    })
  }, [])

  const handleCanvasWheel = useCallback((event) => {
    event.preventDefault()

    const rect = event.currentTarget.getBoundingClientRect()
    const pointerX = event.clientX - rect.left
    const pointerY = event.clientY - rect.top

    setViewport((current) => {
      const delta = event.deltaY < 0 ? 0.11 : -0.11
      const nextZoom = clamp(current.zoom + delta, 0.35, 2.2)
      const ratio = nextZoom / current.zoom

      return {
        zoom: nextZoom,
        x: pointerX - (pointerX - current.x) * ratio,
        y: pointerY - (pointerY - current.y) * ratio,
      }
    })
  }, [])

  const handleCanvasPointerDown = useCallback(
    (event) => {
      if (event.button !== 0) return

      panRef.current = {
        active: true,
        pointerId: event.pointerId,
        startX: event.clientX,
        startY: event.clientY,
        originX: viewport.x,
        originY: viewport.y,
      }

      setIsPanning(true)
      event.currentTarget.setPointerCapture(event.pointerId)
    },
    [viewport.x, viewport.y],
  )

  const handleCanvasPointerMove = useCallback((event) => {
    const pan = panRef.current
    if (!pan.active || pan.pointerId !== event.pointerId) return

    const deltaX = event.clientX - pan.startX
    const deltaY = event.clientY - pan.startY

    setViewport((current) => ({
      ...current,
      x: pan.originX + deltaX,
      y: pan.originY + deltaY,
    }))
  }, [])

  const endPan = useCallback((event) => {
    const pan = panRef.current

    if (!pan.active || pan.pointerId !== event.pointerId) return

    panRef.current.active = false
    setIsPanning(false)

    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }, [])

  return (
    <div className="app-shell">
      <header className="topbar">
        <div>
          <h1>Seesaw Insight</h1>
          <p>Two panes: workflows on the left, flow canvas on the right.</p>
        </div>

        <div className="topbar-metrics">
          <span className={`connection ${connected ? 'live' : ''}`}>
            <span className="dot" />
            {connected ? 'Live stream' : 'Reconnecting'}
          </span>
          <span>events {safeLocale(stats.total_events)}</span>
          <span>active {safeLocale(stats.active_effects)}</span>
          <span>failed {safeLocale(stats.failed_effects)}</span>
        </div>
      </header>

      <main className="two-pane">
        <aside className="panel workflows-pane">
          <header className="panel-header">
            <h2>Workflows</h2>
            <span>{sortedWorkflows.length}/{workflowFilterCounts.all}</span>
          </header>

          <div className="panel-tools">
            <input
              value={workflowQuery}
              onChange={(event) => setWorkflowQuery(event.target.value)}
              placeholder="Search correlation id / event / error"
            />
          </div>

          <div className="workflow-filters">
            <button
              type="button"
              className={`filter-chip ${workflowView === 'all' ? 'active' : ''}`}
              onClick={() => setWorkflowView('all')}
            >
              All {workflowFilterCounts.all}
            </button>
            <button
              type="button"
              className={`filter-chip ${workflowView === 'failed' ? 'active' : ''}`}
              onClick={() => setWorkflowView('failed')}
            >
              Failed {workflowFilterCounts.failed}
            </button>
            <button
              type="button"
              className={`filter-chip ${workflowView === 'dead' ? 'active' : ''}`}
              onClick={() => setWorkflowView('dead')}
            >
              Dead {workflowFilterCounts.dead}
            </button>
          </div>

          <div className="workflow-list">
            {sortedWorkflows.length === 0 && (
              <div className="empty-state">
                <h3>{workflowFilterCounts.all === 0 ? 'Waiting for traffic' : 'No workflows in this filter'}</h3>
                <p>
                  {workflowFilterCounts.all === 0
                    ? 'Workflows appear here as events stream in.'
                    : 'Switch filter or adjust search to find matching workflows.'}
                </p>
              </div>
            )}

            {sortedWorkflows.map((workflow) => {
              const pinned = pinnedWorkflowIds.includes(workflow.correlation_id)
              const failedCount = workflowFailedCount(workflow)
              const rootBatchLabel = batchProgressLabel(
                workflow.root?.batchIndex,
                workflow.root?.batchSize,
              )

              return (
                <article
                  key={workflow.correlation_id}
                  className={`workflow-card ${
                    workflow.correlation_id === selectedWorkflowId ? 'selected' : ''
                  }`}
                >
                  <button
                    type="button"
                    className="workflow-main"
                    onClick={() => setSelectedWorkflowId(workflow.correlation_id)}
                  >
                    <div className="workflow-head">
                      <strong>{workflow.root?.type || 'Workflow'}</strong>
                      <time>{formatTimestamp(workflow.latest_at)}</time>
                    </div>

                    <code className="workflow-id" title={workflow.correlation_id}>
                      {workflow.correlation_id}
                    </code>

                    <div className="workflow-metrics">
                      <span>{workflow.event_count} events</span>
                      <span className={failedCount > 0 ? 'tone-fail' : 'tone-neutral'}>
                        {failedCount} failed
                      </span>
                      <span className={workflow.dead_letters > 0 ? 'tone-fail' : 'tone-neutral'}>
                        {workflow.dead_letters} dead
                      </span>
                      {rootBatchLabel && <span className="tone-warn">{rootBatchLabel}</span>}
                    </div>

                    {workflow.failed?.last_error && (
                      <p className="workflow-error">{workflow.failed.last_error}</p>
                    )}
                  </button>

                  <button
                    type="button"
                    className={`pin-toggle ${pinned ? 'active' : ''}`}
                    onClick={() => togglePinned(workflow.correlation_id)}
                    title={pinned ? 'Unpin workflow' : 'Pin workflow'}
                  >
                    {pinned ? 'Pinned' : 'Pin'}
                  </button>
                </article>
              )
            })}
          </div>
        </aside>

        <section className="panel canvas-pane">
          <header className="panel-header">
            <h2>Cascade Canvas</h2>
            {selectedWorkflowId ? (
              <code title={selectedWorkflowId}>{shortId(selectedWorkflowId)}</code>
            ) : (
              <span>Select workflow</span>
            )}
          </header>

          <div className="canvas-toolbar">
            <div className="toolbar-group">
              <span>{tree?.event_count ?? 0} events</span>
              <span>{tree?.effect_count ?? 0} effects</span>
              <span>{Array.isArray(tree?.roots) ? tree.roots.length : 0} roots</span>
            </div>

            <div className="toolbar-group">
              <button type="button" onClick={() => adjustZoom(-0.12)}>
                −
              </button>
              <button
                type="button"
                onClick={() => {
                  setViewport({ x: 44, y: 52, zoom: 1 })
                  setIsPanning(false)
                }}
              >
                Reset
              </button>
              <button type="button" onClick={() => adjustZoom(0.12)}>
                +
              </button>
              <button type="button" onClick={fitCanvas}>
                Fit
              </button>
            </div>
          </div>

          <div
            ref={canvasViewportRef}
            className={`canvas-viewport ${isPanning ? 'panning' : ''}`}
            onWheel={handleCanvasWheel}
            onPointerDown={handleCanvasPointerDown}
            onPointerMove={handleCanvasPointerMove}
            onPointerUp={endPan}
            onPointerCancel={endPan}
          >
            {!selectedWorkflowId && (
              <div className="empty-state center">
                <h3>No workflow selected</h3>
                <p>Choose a correlation id from the left to open its flow canvas.</p>
              </div>
            )}

            {selectedWorkflowId && treeLoading && !tree && (
              <div className="empty-state center">
                <h3>Loading tree</h3>
                <p>Fetching workflow cascade.</p>
              </div>
            )}

            {selectedWorkflowId && treeError && (
              <div className="empty-state center error">
                <h3>Tree unavailable</h3>
                <p>{treeError}</p>
              </div>
            )}

            {selectedWorkflowId && tree && !treeError && graph.nodes.length === 0 && (
              <div className="empty-state center">
                <h3>No tree nodes yet</h3>
                <p>This workflow has no parent-child event links yet.</p>
              </div>
            )}

            {selectedWorkflowId && tree && !treeError && graph.nodes.length > 0 && (
              <div
                className="canvas-stage"
                style={{
                  width: `${graph.width}px`,
                  height: `${graph.height}px`,
                  transform: `translate(${viewport.x}px, ${viewport.y}px) scale(${viewport.zoom})`,
                }}
              >
                <svg className="canvas-edges" width={graph.width} height={graph.height}>
                  {graph.edges.map((edge) => {
                    const source = graph.nodeMap.get(edge.source)
                    const target = graph.nodeMap.get(edge.target)
                    if (!source || !target) return null

                    const active = selectedNodeId === edge.source || selectedNodeId === edge.target

                    return (
                      <path
                        key={edge.id}
                        d={edgePath(source, target)}
                        className={`canvas-edge ${active ? 'active' : ''}`}
                      />
                    )
                  })}
                </svg>

                <div className="canvas-nodes">
                  {graph.nodes.map((node) => {
                    const event = node.raw
                    const effects = Array.isArray(event.effects) ? event.effects : []
                    const failedEffects = effects.filter((effect) => effect.status === 'failed').length
                    const joinWaitingEffects = effects.filter((effect) =>
                      isJoinWaitingResult(effect?.result),
                    ).length
                    const batchLabel = batchProgressLabel(event.batch_index, event.batch_size)
                    const active = selectedNodeId === node.id

                    return (
                      <button
                        key={node.id}
                        type="button"
                        className={`canvas-node ${active ? 'selected' : ''}`}
                        style={{ transform: `translate(${node.x}px, ${node.y}px)` }}
                        onClick={() => setSelectedNodeId(node.id)}
                        onPointerDown={(event) => event.stopPropagation()}
                      >
                        <header>
                          <strong>{simplifyType(event.event_type)}</strong>
                          <time>{formatTimestamp(event.created_at)}</time>
                        </header>

                        <div className="node-meta">
                          <code title={event.event_id}>{shortId(event.event_id)}</code>
                          <span>{Array.isArray(event.children) ? event.children.length : 0} children</span>
                        </div>

                        <div className="node-badges">
                          <span>{effects.length} effects</span>
                          <span className={failedEffects > 0 ? 'tone-fail' : 'tone-neutral'}>
                            {failedEffects} failed
                          </span>
                          {batchLabel && <span className="tone-warn">{batchLabel}</span>}
                          {joinWaitingEffects > 0 && (
                            <span className="tone-warn">join waiting</span>
                          )}
                        </div>
                      </button>
                    )
                  })}
                </div>
              </div>
            )}

            {selectedNode && (
              <aside
                className="canvas-inspector"
                onPointerDown={(event) => event.stopPropagation()}
                onWheel={(event) => event.stopPropagation()}
              >
                <header>
                  <h3>{simplifyType(selectedNode.raw.event_type)}</h3>
                  <span>{formatTimestamp(selectedNode.raw.created_at)}</span>
                </header>

                <div className="inspector-meta">
                  <code title={selectedNode.raw.event_id}>{selectedNode.raw.event_id}</code>
                  <span>{selectedNodeChildren} children</span>
                  <span>{selectedNodeEffects.length} effects</span>
                  {selectedNodeBatchLabel && <span>{selectedNodeBatchLabel}</span>}
                  {selectedNode.raw.batch_id && (
                    <code title={selectedNode.raw.batch_id}>
                      batch {shortId(selectedNode.raw.batch_id)}
                    </code>
                  )}
                </div>

                <details open>
                  <summary>event payload</summary>
                  <pre>{jsonPretty(selectedNode.raw.payload) || 'No payload'}</pre>
                </details>

                <details>
                  <summary>effects on this event</summary>
                  {selectedNodeEffects.length === 0 ? (
                    <p className="muted-copy">No effects executed from this event.</p>
                  ) : (
                    <ul className="inspector-list">
                      {selectedNodeEffects.map((effect) => (
                        <li key={`${effect.effect_id}-${effect.event_id}`}>
                          <div className="row-head">
                            <code>{effect.effect_id}</code>
                            <div className="row-pills">
                              <span className={`pill ${effectTone(effect.status)}`}>{effect.status}</span>
                              {isJoinWaitingResult(effect.result) && (
                                <span className="pill tone-warn">join waiting</span>
                              )}
                            </div>
                          </div>
                          <div className="row-meta">
                            <span>attempts {effect.attempts}</span>
                            {batchProgressLabel(effect.batch_index, effect.batch_size) && (
                              <span>{batchProgressLabel(effect.batch_index, effect.batch_size)}</span>
                            )}
                            {effect.error && <span className="tone-fail">{effect.error}</span>}
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}
                </details>

                <details>
                  <summary>workflow operations</summary>
                  <div className="ops-mini-grid">
                    <span>avg {timingStats.avg ?? '—'}ms</span>
                    <span>p95 {timingStats.p95 ?? '—'}ms</span>
                    <span>max {timingStats.max ?? '—'}ms</span>
                    <span>dead {selectedWorkflowDeadLetters.length}</span>
                    <span>batches {selectedWorkflowBatchStats.batches}</span>
                    <span>items {selectedWorkflowBatchStats.items}</span>
                    <span>join waiting {selectedWorkflowBatchStats.joinWaiting}</span>
                  </div>

                  {effectLogs.length > 0 && (
                    <ul className="inspector-list compact">
                      {effectLogs.slice(0, 10).map((log) => (
                        <li key={`${log.effect_id}-${log.event_id}`}>
                          <div className="row-head">
                            <code>{log.effect_id}</code>
                            <div className="row-pills">
                              <span className={`pill ${effectTone(log.status)}`}>{log.status}</span>
                              {isJoinWaitingResult(log.result) && (
                                <span className="pill tone-warn">join waiting</span>
                              )}
                            </div>
                          </div>
                          <div className="row-meta">
                            <span>{simplifyType(log.event_type)}</span>
                            <span>{log.duration_ms ?? '—'}ms</span>
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}

                  {selectedWorkflowDeadLetters.length > 0 && (
                    <ul className="inspector-list compact">
                      {selectedWorkflowDeadLetters.slice(0, 6).map((row) => (
                        <li key={`${row.effect_id}-${row.failed_at}`}>
                          <div className="row-head">
                            <code>{row.effect_id}</code>
                            <span className="pill tone-fail">{row.reason}</span>
                          </div>
                          <div className="row-meta">
                            <span>{simplifyType(row.event_type)}</span>
                            <span>{formatTimestamp(row.failed_at)}</span>
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}
                </details>

                {selectedWorkflow && (
                  <div className="inspector-footer">
                    <span>{streamLabel('event_dispatched')} root: {selectedWorkflow.root?.type || 'n/a'}</span>
                    <span>{selectedWorkflow.event_count} total events</span>
                  </div>
                )}
              </aside>
            )}
          </div>
        </section>
      </main>
    </div>
  )
}

export default App
