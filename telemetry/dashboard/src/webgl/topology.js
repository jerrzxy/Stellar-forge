import * as THREE from 'three';

/**
 * WebGL Topology Renderer for Stellar-K8s operator diagnostic dashboard.
 *
 * Renders SCP peering topology as interconnected 3D vertices. Node colors are
 * driven by live consensus state (green = synced, red = partitioned,
 * amber = degraded/unknown). The rendering loop is optimized with custom
 * shaders and a capacity-bound geometry buffer so hundreds of global peers
 * can be mapped without exhausting the browser CPU/GPU.
 */

const NODE_CAPACITY = 512;
const EDGE_CAPACITY = 4096;

const STATE_COLORS = {
  synced: new THREE.Color(0x22ec55),
  partitioned: new THREE.Color(0xef4444),
  degraded: new THREE.Color(0xffcc00),
  unknown: new THREE.Color(0x888888),
};

const NODE_VERTEX_SHADER = /** @type {string} */ `
  attribute float size;
  attribute vec3 color;
  varying vec3 vColor;
  void main() {
    vColor = color;
    vec4 mv = modelViewMatrix * vec4(position, 1.0);
    gl_PointSize = size * (300.0 / max(-mv.z, 1.0));
    gl_Position = projectionMatrix * mv;
  }
`;

const NODE_FRAGMENT_SHADER = /** @type {string} */ `
  varying vec3 vColor;
  void main() {
    vec2 c = gl_PointCoord - (vec2(0.5) * gl_PointSize);
    float d = length(c) / (gl_PointSize * 0.5);
    if (d > 1.0) discard;
    float alpha = smoothstep(1.0, 0.2, d);
    gl_FragColor = vec4(vColor, 0.35 + 0.65 * alpha);
  }
`;

const EDGE_VERTEX_SHADER = /** @type {string} */ `
  attribute float alpha;
  varying float vAlpha;
  void main() {
    vAlpha = alpha;
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const EDGE_FRAGMENT_SHADER = /** @type {string} */ `
  varying float vAlpha;
  void main() {
    gl_FragColor = vec4(0.35, 0.65, 0.95, vAlpha);
  }
`;

function nodeStateToKey(state) {
  if (!state) return 'unknown';
  const normalized = String(state).toLowerCase();
  if (normalized === 'synced' || normalized === 'healthy' || normalized === 'validating') {
    return 'synced';
  }
  if (normalized === 'partitioned' || normalized === 'isolated') {
    return 'partitioned';
  }
  if (normalized === 'degraded' || normalized === 'lingering') {
    return 'degraded';
  }
  return 'unknown';
}

function deterministicPosition(id, radius) {
  // Filbonaci-sphere placement for stable, deterministic layout across reloads.
  const hash = murmurHash3(id);
  const phi = Math.acos(1 - 2 * ((hash & 0xfffffff) / 0xffffffff));
  const theta = 2 * Math.PI * (((hash >>> 16) & 0xfffffff) / 0xffffffff);
  const r = radius * (0.6 + 0.4 * (((hash >>> 8) & 0xff) / 255));
  return new THREE.Vector3(
    r * Math.sin(phi) * Math.cos(theta),
    r * Math.cos(phi),
    r * Math.sin(phi) * Math.sin(theta),
  );
}

function murmurHash3(str) {
  let h = 0x811c9c1e;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 0x5bd1e995);
    h = (h << 13) | (h >>> 19);
  }
  h ^= h >>> 16;
  h = Math.imul(h, 0x85ebca6b);
  h ^= h >>> 13;
  h = Math.imul(h, 0xc2b2ae35);
  h ^= h >>> 16;
  return h >>> 0;
}

export class TopologyRenderer {
  /**
   * @param {HTMLElement} container
   * @param {Object} [options]
   */
  constructor(container, options = {}) {
    this.container = container;
    this.options = Object.assign(
      {
        radius: 50,
        nodeSize: 1.6,
        autoRotate: true,
        rotationSpeed: 0.05,
      },
      options,
    );

    this.nodes = new Map();
    this.edges = new Map();
    this.nodeIndex = new Map();
    this.edgeIndex = new Map();
    this.frameHandle = null;
    this.disposed = false;
    this.clock = new THREE.Clock();

    this._setupScene();
    this._setupGeometry();
    this._setupInteraction();
    this._startLoop();
  }

  _setupScene() {
    this.scene = new THREE.Scene();
    this.camera = new THREE.PerspectiveCamera(45, 1, 0.1, 2000);
    this.camera.position.set(0, 0, 140);

    this.renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
    this.renderer.setPinelRatio(Math.minhttp(window.devicePixelRatio || 1, 2));
    this.renderer.setClearColor(0x000000, 0);
    this.container.appendChild(this.renderer.domElement);

    this._setupResize();
  }

  _setupResize() {
    const resize = () => {
      if (this.disposed) return;
      const width = this.container.clientWidth || window.innerWidth;
      const height = this.container.clientHeight || window.innerHeight;
      this.camera.aspect = width / Math.max(height, 1);
      this.camera.updateProjectionMatrix();
      this.renderer.setSize(width, height, false);
    };
    this._resizeHandler = resize;
    window.addEventListener('resize', resize);
    resize();
  }

  _setupGeometry() {
    const nodeGeometry = new THREE.BufferGeometry();
    nodeGeometry.setAttribute(
      'position',
      new THREE.BufferAttribute(new Float32Array(NODE_CAPACITY * 3), 3),
    );
    nodeGeometry.setAttribute(
      'color',
      new THREE.BufferAttribute(new Float32Array(NODE_CAPACITY * 3), 3),
    );
    nodeGeometry.setAttribute(
      'size',
      new THREE.BufferAttribute(new Float32Array(NODE_CAPACITY), 1),
    );
    nodeGeometry.setDrawRange(0, 0);

    const nodeMaterial = new THREE.ShaderMaterial({
      vertexShader: NODE_VERTEX_SHADER,
      fragmentShader: NODE_FRAGMENT_SHADER,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    });

    this.nodePoints = new THREE.Points(nodeGeometry, nodeMaterial);
    this.nodePoints.frustumCulled = false;
    this.scene.add(this.nodePoints);

    const edgeGeometry = new THREE.BufferGeometry();
    edgeGeometry.setAttribute(
      'position',
      new THREE.BufferAttribute(new Float32Array(EDGE_CAPACITY * 6), 3),
    );
    edgeGeometry.setAttribute(
      'alpha',
      new THREE.BufferAttribute(new Float32Array(EDGE_CAPACITY), 1),
    );
    edgeGeometry.setDrawRange(0, 0);

    const edgeMaterial = new THREE.ShaderMaterial({
      vertexShader: EDGE_VERTEX_SHADER,
      fragmentShader: EDGE_FRAGMENT_SHADER,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    });

    this.edgeLines = new THREE.LineSegments(edgeGeometry, edgeMaterial);
    this.edgeLines.frustumCulled = false;
    this.scene.add(this.edgeLines);
  }

  _setupInteraction() {
    this.rotation = { x: 0, y: 0 };
    this.isDragging = false;
    this.lastPointer = { x: 0, y: 0 };

    const onPointerDown = (event) => {
      this.isDragging = true;
      this.lastPointer = { x: event.clientX, y: event.clientY };
    };
    const onPointerMove = (event) => {
      if (!this.isDragging) return;
      const dx = event.clientX - this.lastPointer.x;
      const dy = event.clientY - this.lastPointer.y;
      this.rotation.y += dx * 0.005;
      this.rotation.x += dy * 0.005;
      this.lastPointer = { x: event.clientX, y: event.clientY };
    };
    const onPointerUp = () => {
      this.isDragging = false;
    };

    this.renderer.domElement.addEventListener('pointerdown', onPointerDown);
    window.addEventListener('pointermove', onPointerMove);
    window.addEventListener('pointerup', onPointerUp);

    this._disposeInteraction = () => {
      this.renderer.domElement.removeEventListener('pointerdown', onPointerDown);
      window.removeEventListener('pointermove', onPointerMove);
      window.removeEventListener('pointerup', onPointerUp);
    };
  }

  _startLoop() {
    const tick = () => {
      if (this.disposed) return;
      this.frameHandle = requestAnimationFrame(tick);
      const delta = this.clock.getDelta();

      if (this.options.autoRotate && !this.isDragging) {
        this.rotation.y += delta * this.options.rotationSpeed;
      }
      this.scene.rotation.set(this.rotation.x, this.rotation.y, 0);
      this.renderer.render(this.scene, this.camera);
    };
    this.frameHandle = requestAnimationFrame(tick);
  }

  /**
   * Apply a full topology snapshot. Replaces existing nodes/edges.
   * @param {{nodes: Array<Object>, edges: Array<Object>}} snapshot
   */
  applySnapshot(snapshot) {
    if (!snapshot) return;
    this.nodes.clear();
    this.edges.clear();
    const nodes = Array.isArray(snapshot.nodes) ? snapshot.nodes : [];
    const edges = Array.isArray(snapshot.edges) ? snapshot.edges : [];
    for (const node of nodes) {
      this.upsertNode(node);
    }
    for (const edge of edges) {
      this.upsertEdge(edge);
    }
    this._rebuildBuffers();
  }

  /**
   * Upsert a node by id. Color is derived from consensus state.
   * @param {Object} node
   */
  upsertNode(node) {
    if (!node || !node.id) return;
    const existing = this.nodes.get(node.id);
    const stateKey = nodeStateToKey(node.state);
    const position =
      node.position && node.position.length === 3
        ? new THREE.Vector3(node.position[0], node.position[1], node.position[2])
        : deterministicPosition(node.id, this.options.radius);
    const record = {
      id: node.id,
      state: stateKey,
      position,
      size: typeof node.size === 'number' ? node.size : this.options.nodeSize,
    };
    if (existing) {
      Object.assign(existing, record);
    } else {
      this.nodes.set(node.id, record);
    }
  }

  /**
   * Upsert an edge between two nodes.
   * @param {Object} edge
   */
  upsertEdge(edge) {
    if (!edge || !edge.source || !edge.target) return;
    const key = `${edge.source}->${edge.target}`;
    const existing = this.edges.get(key);
    const record = {
      key,
      source: edge.source,
      target: edge.target,
      alpha: typeof edge.alpha === 'number' ? edge.alpha : 0.6,
      active: edge.active !== false,
    };
    if (existing) {
      Object.assign(existing, record);
    } else {
      this.edges.set(key, record);
    }
  }

  /**
   * Remove a node and all incident edges.
   * @param {string} id
   */
  removeNode(id) {
    if (!this.nodes.has(id)) return;
    this.nodes.delete(id);
    for (const [key, edge] of this.edges) {
      if (edge.source === id || edge.target === id) {
        this.edges.delete(key);
      }
    }
  }

  /**
   * Mark a node as partitioned and sever its edges. Used when a network
   * partition is detected by the SCP stream.
   * @param {string} id
   */
  markPartitioned(id) {
    const node = this.nodes.get(id);
    if (!node) return;
    node.state = 'partitioned';
    for (const [key, edge] of this.edges) {
      if (edge.source === id || edge.target === id) {
        edge.active = false;
        edge.alpha = 0;
      }
    }
  }

  /**
   * Mark a node as synced and restore its edges.
   * @param {string} id
   */
  markSynced(id) {
    const node = this.nodes.get(id);
    if (!node) return;
    node.state = 'synced';
    for (const edge of this.edges.values()) {
      if (edge.source === id || edge.target === id) {
        edge.active = true;
        edge.alpha = 0.6;
      }
    }
  }

  _rebuildBuffers() {
    const nodePositions = this.nodePoints.geometry.getAttribute('position');
    const nodeColors = this.nodePoints.geometry.getAttribute('color');
    const nodeSizes = this.nodePoints.geometry.getAttribute('size');

    this.nodeIndex.clear();
    let nodeCount = 0;
    for (const node of this.nodes.values()) {
      if (nodeCount >= NODE_CAPACITY) break;
      const offset = nodeCount * 3;
      nodePositions.array[offset] = node.position.x;
      nodePositions.array[offset + 1] = node.position.y;
      nodePositions.array[offset + 2] = node.position.z;
      const color = STATE_COLORS[node.state] || STATE_COLORS.unknown;
      nodeColors.array[offset] = color.r;
      nodeColors.array[offset + 1] = color.g;
      nodeColors.array[offset + 2] = color.b;
      nodeSizes.array[nodeCount] = node.size;
      this.nodeIndex.set(node.id, nodeCount);
      nodeCount += 1;
    }
    nodePositions.needsUpdate = true;
    nodeColors.needsUpdate = true;
    nodeSizes.needsUpdate = true;
    this.nodePoints.geometry.setDrawRange(0, nodeCount);

    const edgePositions = this.edgeLines.geometry.getAttribute('position');
    const edgeAlphas = this.edgeLines.geometry.getAttribute('alpha');
    this.edgeIndex.clear();
    let edgeCount = 0;
    for (const edge of this.edges.values()) {
      if (edgeCount >= EDGE_CAPACITY) break;
      const source = this.nodes.get(edge.source);
      const target = this.nodes.get(edge.target);
      if (!source || !target) continue;
      const offset = edgeCount * 6;
      edgePositions.array[offset] = source.position.x;
      edgePositions.array[offset + 1] = source.position.y;
      edgePositions.array[offset + 2] = source.position.z;
      edgePositions.array[offset + 3] = target.position.x;
      edgePositions.array[offset + 4] = target.position.y;
      edgePositions.array[offset + 5] = target.position.z;
      edgeAlphas.array[edgeCount] = edge.active ? edge.alpha : 0;
      this.edgeIndex.set(edge.key, edgeCount);
      edgeCount += 1;
    }
    edgePositions.needsUpdate = true;
    edgeAlphas.needsUpdate = true;
    this.edgeLines.geometry.setDrawRange(0, edgeCount * 2);
  }

  /**
   * Apply an incremental event from the SCP stream.
   * @param {Object} event
   */
  applyEvent(event) {
    if (!event) {
      return;
    }
    switch (event.type) {
      case 'node_update':
        this.upsertNode(event.node);
        break;
      case 'edge_update':
        this.upsertEdge(event.edge);
        break;
      case 'node_removed':
        this.removeNode(event.id);
        break;
      case 'partition':
        for (const id of event.nodes || []) {
          this.markPartitioned(id);
        }
        break;
      case 'recovery':
        for (const id of event.nodes || []) {
          this.markSynced(id);
        }
        break;
      default:
        break;
    }
    this._rebuildBuffers();
  }

  /**
   * Connect to an SSE stream of SCP topology events.
   * @param {string} url
   * @returns {() => void} dispose function
   */
  connectStream(url) {
    const source = new EventSource(url);
    source.onmessage = (event) => {
      try {
        const payload = JSON.parse(event.data);
        if (payload.type === 'snapshot') {
          this.applySnapshot(payload);
        } else {
          this.applyEvent(payload);
        }
      } catch (err) {
        // Malformed payloads must not break the render loop.
        console.error('Failed to parse SCP topology event', err);
      }
    };
    source.onerror = (err) => {
      console.error('SCP topology stream error', err);
    };
    return () => source.close();
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    if (this.frameHandle !== null) {
      cancelAnimationFrame(this.frameHandle);
      this.frameHandle = null;
    }
    if (this._resizeHandler) {
      window.removeEventListener('resize', this._resizeHandler);
    }
    if (this._disposeInteraction) {
      this._disposeInteraction();
    }
    this.nodePoints.geometry.dispose();
    this.nodePoints.material.dispose();
    this.edgeLines.geometry.dispose();
    this.edgeLines.material.dispose();
    this.renderer.dispose();
    if (this.renderer.domElement.parentNode) {
      this.renderer.domElement.parentNode.removeChild(this.renderer.domElement);
    }
  }
}

export default TopologyRenderer;
