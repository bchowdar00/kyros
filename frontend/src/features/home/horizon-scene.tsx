'use client'

import { useEffect, useRef } from 'react'

/** How long the light takes to arrive, in seconds. */
const REVEAL_SECONDS = 1.6
const PLANET_RADIUS = 7
/** The atmosphere's shell, as a multiple of the planet's radius. */
const SHELL = 1.13
/** The deepest the camera's view runs into the shell on the far side (see the halo shader). */
const SHELL_GRAZE = Math.sqrt(1 - 1 / (SHELL * SHELL))
const STAR_COUNT = 520
const MAX_PIXEL_RATIO = 2

const planetVertex = /* glsl */ `
  varying vec3 vNormal;
  varying vec3 vViewDir;

  void main() {
    vec4 mvPosition = modelViewMatrix * vec4(position, 1.0);
    vNormal = normalMatrix * normal;
    vViewDir = -mvPosition.xyz;
    gl_Position = projectionMatrix * mvPosition;
  }
`

/** The night side of a planet: ink, with a thin violet line where it meets the sky. */
const planetFragment = /* glsl */ `
  uniform vec3 uInk;
  uniform vec3 uRim;
  uniform float uReveal;
  varying vec3 vNormal;
  varying vec3 vViewDir;

  void main() {
    float facing = max(dot(normalize(vViewDir), normalize(vNormal)), 0.0);
    float rim = pow(1.0 - facing, 5.0);
    gl_FragColor = vec4(mix(uInk, uRim, rim * uReveal), 1.0);
    #include <tonemapping_fragment>
    #include <colorspace_fragment>
  }
`

/**
 * The atmosphere: the inside of a larger shell, brightest where the line of sight grazes
 * the planet and gone by the shell's own edge.
 */
const haloFragment = /* glsl */ `
  uniform vec3 uGlow;
  uniform float uGraze;
  uniform float uStrength;
  varying vec3 vNormal;
  varying vec3 vViewDir;

  void main() {
    float away = max(-dot(normalize(vViewDir), normalize(vNormal)), 0.0);
    float depth = clamp(away / uGraze, 0.0, 1.0);
    float glow = pow(depth, 2.4) * uStrength;
    gl_FragColor = vec4(uGlow, glow);
    #include <tonemapping_fragment>
    #include <colorspace_fragment>
  }
`

/** Exponential ease-out: quick to arrive, slow to settle. */
function easeOut(progress: number): number {
  return progress >= 1 ? 1 : 1 - 2 ** (-10 * progress)
}

/**
 * The light behind the Markets heading: the limb of a dark planet across the top of the
 * page, its violet atmosphere and a field of stars, drawn with three.js. Decoration only. It
 * arrives once, then breathes slowly and leans a little toward the pointer; with reduced
 * motion it is a still picture, and where WebGL is missing nothing is drawn and the page's
 * own glow (globals.css) stands alone. three.js is loaded only when this mounts.
 */
export function HorizonScene() {
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    if (canvas === null) return
    let dispose: (() => void) | undefined
    let cancelled = false

    void import('three')
      .then(THREE => {
        if (cancelled) return
        const renderer = new THREE.WebGLRenderer({ alpha: true, antialias: true, canvas })
        renderer.setPixelRatio(Math.min(window.devicePixelRatio, MAX_PIXEL_RATIO))

        const scene = new THREE.Scene()
        const camera = new THREE.PerspectiveCamera(35, 1, 0.1, 100)
        camera.position.set(0, 0, 6)
        // Half the height the camera sees at the planet's distance.
        const halfHeight = 6 * Math.tan(THREE.MathUtils.degToRad(35 / 2))
        const restY = halfHeight + PLANET_RADIUS - 0.85

        const sphere = new THREE.SphereGeometry(PLANET_RADIUS, 96, 96)
        // The two values the frame loop drives, held here so it need not look them up.
        const revealUniform = { value: 0 }
        const strengthUniform = { value: 0 }
        const planetMaterial = new THREE.ShaderMaterial({
          fragmentShader: planetFragment,
          uniforms: {
            uInk: { value: new THREE.Color(0x07060d) },
            uReveal: revealUniform,
            uRim: { value: new THREE.Color(0xa78bff) },
          },
          vertexShader: planetVertex,
        })
        const haloMaterial = new THREE.ShaderMaterial({
          depthWrite: false,
          fragmentShader: haloFragment,
          side: THREE.BackSide,
          transparent: true,
          uniforms: {
            uGlow: { value: new THREE.Color(0x6d4bff) },
            uGraze: { value: SHELL_GRAZE },
            uStrength: strengthUniform,
          },
          vertexShader: planetVertex,
        })
        const planet = new THREE.Mesh(sphere, planetMaterial)
        const halo = new THREE.Mesh(sphere, haloMaterial)
        halo.scale.setScalar(SHELL)
        const body = new THREE.Group()
        body.add(planet, halo)
        scene.add(body)

        const positions = new Float32Array(STAR_COUNT * 3)
        for (let index = 0; index < STAR_COUNT; index += 1) {
          positions[index * 3] = (Math.random() - 0.5) * 22
          positions[index * 3 + 1] = (Math.random() - 0.5) * 6
          positions[index * 3 + 2] = -1 - Math.random() * 7
        }
        const starGeometry = new THREE.BufferGeometry()
        starGeometry.setAttribute('position', new THREE.BufferAttribute(positions, 3))
        const starMaterial = new THREE.PointsMaterial({
          color: 0xcfc4ff,
          depthWrite: false,
          opacity: 0,
          size: 0.022,
          transparent: true,
        })
        const stars = new THREE.Points(starGeometry, starMaterial)
        scene.add(stars)

        const still = window.matchMedia('(prefers-reduced-motion: reduce)').matches
        const pointer = { x: 0, y: 0 }
        const timer = new THREE.Timer()
        timer.connect(document)

        const draw = (elapsed: number) => {
          const reveal = still ? 1 : easeOut(elapsed / REVEAL_SECONDS)
          const breath = still ? 1 : 0.92 + 0.08 * Math.sin(elapsed * 0.45)
          revealUniform.value = reveal
          strengthUniform.value = 0.85 * reveal * breath
          starMaterial.opacity = 0.7 * reveal
          // The planet settles down into place as the light arrives.
          body.position.y = restY + (1 - reveal) * 0.45
          stars.rotation.z = elapsed * 0.004
          camera.position.x += (pointer.x * 0.18 - camera.position.x) * 0.04
          camera.position.y += (pointer.y * 0.08 - camera.position.y) * 0.04
          camera.lookAt(0, 0, 0)
          renderer.render(scene, camera)
        }

        const resize = () => {
          const { clientHeight, clientWidth } = canvas
          if (clientWidth === 0 || clientHeight === 0) return
          renderer.setSize(clientWidth, clientHeight, false)
          camera.aspect = clientWidth / clientHeight
          camera.updateProjectionMatrix()
          if (still) draw(REVEAL_SECONDS)
        }
        const observer = new ResizeObserver(resize)
        observer.observe(canvas)
        resize()

        const onPointer = (event: PointerEvent) => {
          pointer.x = (event.clientX / window.innerWidth) * 2 - 1
          pointer.y = -((event.clientY / window.innerHeight) * 2 - 1)
        }
        // A tab nobody is looking at draws nothing.
        const onVisibility = () => {
          renderer.setAnimationLoop(document.hidden || still ? null : loop)
        }
        const loop = () => {
          timer.update()
          draw(timer.getElapsed())
        }
        if (!still) {
          window.addEventListener('pointermove', onPointer, { passive: true })
          document.addEventListener('visibilitychange', onVisibility)
          renderer.setAnimationLoop(loop)
        }

        dispose = () => {
          renderer.setAnimationLoop(null)
          observer.disconnect()
          window.removeEventListener('pointermove', onPointer)
          document.removeEventListener('visibilitychange', onVisibility)
          timer.dispose()
          sphere.dispose()
          starGeometry.dispose()
          planetMaterial.dispose()
          haloMaterial.dispose()
          starMaterial.dispose()
          renderer.dispose()
        }
        if (cancelled) dispose()
      })
      // No WebGL, or three.js could not be fetched: the page keeps its plain glow.
      .catch(() => undefined)

    return () => {
      cancelled = true
      dispose?.()
    }
  }, [])

  return (
    <canvas
      aria-hidden="true"
      className="pointer-events-none absolute top-app-nav left-0 -z-[1] h-[26rem] w-full"
      ref={canvasRef}
    />
  )
}
