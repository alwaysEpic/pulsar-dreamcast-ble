"""
Parametric build of the Pulsar VMU enclosure - BIG CELL variant.

Same build as build_pulsar_vmu.py except the battery pocket: instead of a
31x36 box with a flat 6.35 ceiling, the front dome is hollowed across its
whole usable footprint with the notch ceiling FOLLOWING THE MEASURED OUTER
SKIN at a constant NOTCH_WALL.  The raw dome is ~3.1 mm solid at the crown
(inner 5.40 / outer 8.50), so this reclaims ~2.1 mm of headroom at x=0 and
widens the pocket from x +-15.5 to wherever the cavity actually ends.

Rear shell, supports, plunger are identical to the 703035 build.  Outputs go
to Edited_VMU_stls/PulsarFit_bigcell/ - the 703035 set is NOT touched.

Run inside Blender (GUI via MCP, or --background).

Board frame -> world:   x_w = lx + OX      y_w = -ly + OY      z_w = -lz + OZ
(the board is rotated 180 deg about X, so the component side faces the REAR half)
"""
import bpy, bmesh, math, os
from mathutils import Vector
from mathutils.bvhtree import BVHTree

# Paths are resolved from this script's location, so the repo works wherever
# it is cloned. Blender's text editor does not set __file__; fall back to bpy.
_HERE = os.path.dirname(os.path.abspath(
    __file__ if "__file__" in globals() else bpy.data.filepath))
_3D   = os.path.dirname(_HERE)

BF   = _HERE + "/"
STL  = os.path.join(_3D, "pulsar_pcb.stl")
OUT  = os.path.join(_3D, "Edited_VMU_stls", "PulsarFit_bigcell") + "/"

# ---------------------------------------------------------------- parameters
OX, OY, OZ   = 0.00, -5.55, -1.36   # board origin.  OY pulled back from -6.55 so the
                                    # USB sits ~1.0 mm inside the nose and can be wrapped.
REAR_WALL    = 0.80                 # left under the JST pockets
POCKET_CL    = 0.90                 # lateral clearance around each connector
USB_CL       = 0.35                 # even gap all round the USB-C shell
FLOOR        = -7.25                # general rear floor
PAD_TOP      = -2.49                # support pads stop just under the board
REAR_SKIN    = -9.25

NOTCH_WALL   = 1.0                  # material left between notch ceiling and outer skin
NOTCH_FLOOR  = 4.5                  # below the raw interior ceiling (5.30 min), above USB
NOTCH_XMAX   = 17.2                 # hard clamp: rib band starts at x +-17.5
NOTCH_YCLAMP = 30.5                 # outer skin still flat here; connector end beyond
SLICE        = 0.5                  # x sampling pitch for the stepped ceiling
CELL_GAP     = 0.15                 # cell sits this far above whatever it rests on

# ghost cells for the clearance report (name, thickness, width, length)
CELLS = [('KBT_603048', 6.0, 30.0, 48.0),
         ('YDL_523450', 5.0, 34.0, 50.0)]

CONNS = {'CN1': (7.47, -23.08, 13.9, 8.2),      # 5-pin, Maple cable
         'CN2': (-10.50, -23.20, 7.9, 8.6),     # battery JST PH
         'CN3': (-6.95, 13.70, 7.9, 8.6)}
SW1_L = (0.0, 23.70)                            # tact switch, board-local
SW1_TOP_L = 3.195                               # its actuator top, board-local z

def Wx(lx): return lx + OX
def Wy(ly): return -ly + OY
def Wz(lz): return -lz + OZ

# ---------------------------------------------------------------- helpers
def purge():
    for o in list(bpy.data.objects):
        if o.type == 'MESH':
            bpy.data.objects.remove(o, do_unlink=True)

def bring(path, name, newname):
    with bpy.data.libraries.load(BF + path, link=False) as (df, dt):
        dt.objects = [name]
    ob = dt.objects[0]
    bpy.context.scene.collection.objects.link(ob)
    ob.name = newname
    return ob

def box(name, x0, x1, y0, y1, z0, z1):
    if name in bpy.data.objects:
        bpy.data.objects.remove(bpy.data.objects[name], do_unlink=True)
    me = bpy.data.meshes.new(name); ob = bpy.data.objects.new(name, me)
    bpy.context.scene.collection.objects.link(ob)
    bm = bmesh.new(); bmesh.ops.create_cube(bm, size=1.0)
    for v in bm.verts:
        v.co.x = x0 if v.co.x < 0 else x1
        v.co.y = y0 if v.co.y < 0 else y1
        v.co.z = z0 if v.co.z < 0 else z1
    bm.to_mesh(me); bm.free()
    ob.display_type = 'WIRE'
    return ob

def cyl(name, x, y, r, z0, z1, seg=32):
    if name in bpy.data.objects:
        bpy.data.objects.remove(bpy.data.objects[name], do_unlink=True)
    me = bpy.data.meshes.new(name); ob = bpy.data.objects.new(name, me)
    bpy.context.scene.collection.objects.link(ob)
    bm = bmesh.new()
    bmesh.ops.create_cone(bm, cap_ends=True, cap_tris=False, segments=seg,
                          radius1=r, radius2=r, depth=(z1 - z0))
    bm.to_mesh(me); bm.free()
    ob.location = (x, y, (z0 + z1) / 2)
    return ob

def boolean(target, cutter, op='DIFFERENCE'):
    """Always start from a clean modifier stack - a stale stack silently
    applies the boolean to the wrong base mesh."""
    c = bpy.data.objects[cutter]
    c.hide_viewport = False
    c.update_tag()
    bpy.context.view_layer.update()
    bpy.context.evaluated_depsgraph_get()      # force the mesh to actually evaluate
    t = bpy.data.objects[target]
    for m in list(t.modifiers):
        t.modifiers.remove(m)
    bpy.context.view_layer.objects.active = t
    m = t.modifiers.new('b', 'BOOLEAN')
    m.operation = op
    m.object = bpy.data.objects[cutter]
    m.solver = 'EXACT'
    bpy.ops.object.modifier_apply(modifier=m.name)

def caster(name):
    dg = bpy.context.evaluated_depsgraph_get()
    o = bpy.data.objects[name]
    b = bmesh.new(); b.from_object(o, dg); b.transform(o.matrix_world)
    t = BVHTree.FromBMesh(b); b.free()
    def h(x, y):
        zs, z = [], -60.0
        while True:
            loc, n, i, d = t.ray_cast(Vector((x, y, z)), Vector((0, 0, 1)))
            if loc is None: break
            if loc.z <= z + 1e-4: z += 1e-3; continue
            zs.append(loc.z); z = loc.z + 1e-3
            if len(zs) > 40: break
        return zs
    return h

def cleanup(name, passes=3):
    ob = bpy.data.objects[name]
    for m in list(ob.modifiers): ob.modifiers.remove(m)
    bpy.ops.object.select_all(action='DESELECT')
    ob.select_set(True); bpy.context.view_layer.objects.active = ob
    bpy.ops.object.mode_set(mode='EDIT')
    bpy.ops.mesh.select_all(action='SELECT')
    # Gentle only.  An aggressive select_non_manifold + fill_holes loop *creates*
    # bad geometry here (it went 16 -> 84 non-manifold edges), so don't.
    bpy.ops.mesh.remove_doubles(threshold=1e-4)
    bpy.ops.mesh.dissolve_degenerate(threshold=1e-4)
    bpy.ops.mesh.delete_loose()
    bpy.ops.mesh.select_all(action='SELECT')
    bpy.ops.mesh.normals_make_consistent(inside=False)
    bpy.ops.object.mode_set(mode='OBJECT')
    try:
        import addon_utils
        addon_utils.enable("object_print3d_utils", default_set=False)
        bpy.ops.object.select_all(action='DESELECT')
        ob.select_set(True); bpy.context.view_layer.objects.active = ob
        bpy.ops.mesh.print3d_clean_non_manifold()
    except Exception as e:
        print("  print3d repair unavailable on %s: %s" % (name, e))
    bm = bmesh.new(); bm.from_mesh(ob.data)
    nm = sum(1 for e in bm.edges if len(e.link_faces) != 2); bm.free()
    return nm

# ---------------------------------------------------------------- 1. shells
purge()
rear  = bring("dreamcast_vmu_back_printed_raw.blend", "VMU_RearShell", "VMU_RearShell")
front = bring("dreamcast_vmu_front_printed_final1.blend", "VMU_FrontShell.001", "VMU_FrontShell")
rear.location = (0, 0, 0);   rear.rotation_euler = (0, 0, 0)
front.location = (0, 0, 1.0); front.rotation_euler = (4.712389, 3.141593, 3.141593)

# ---------------------------------------------------------------- 2. board
bpy.ops.object.select_all(action='DESELECT')
bpy.ops.wm.stl_import(filepath=STL)
pcb = bpy.context.selected_objects[0]; pcb.name = 'Pulsar_PCB'
bpy.context.view_layer.objects.active = pcb
pcb.location = (-125.0, -105.41, -4.30)          # STL -> board frame
bpy.ops.object.transform_apply(location=True, rotation=False, scale=False)
pcb.rotation_euler = (math.pi, 0, 0)
pcb.location = (OX, OY, OZ)
pcb.color = (0.10, 0.45, 0.18, 1)
bpy.context.view_layer.update()

HP = caster('Pulsar_PCB')

# ---------------------------------------------------------------- 3. cable notch
c = bring("dreamcast_vmu_back_printed_raw.blend", "Cube.004", "CUT_cable")
c.location = (14.18, 36.48, -0.08)               # transform is lost on append
c.scale = (5.360, 6.980, 4.065)
c.rotation_euler = (0, 0, 0)
c.display_type = 'WIRE'
bpy.context.view_layer.update()
boolean('VMU_RearShell', 'CUT_cable')

# ---------------------------------------------------------------- 4. nose relief
XIAO_TOP = Wz(-1.09)                              # top of the XIAO's own PCB
NOSE_STEP = 0.25                                  # relief slice pitch
prof = []
y = -36.0
while y <= -28.0:
    hwR = 0.0; loR = 99.0; loA = 99.0; hwF = 0.0
    yy = y - NOSE_STEP/2 - 0.05
    while yy <= y + NOSE_STEP/2 + 0.05 + 1e-9:   # sample the whole slice, not its centre
        x = -16.0
        while x <= 16.0:
            h = HP(x, yy)
            if h:
                loA = min(loA, min(h))
                rear_side  = [v for v in h if v <= OZ]
                front_side = [v for v in h if v >  OZ]
                if rear_side:
                    hwR = max(hwR, abs(x)); loR = min(loR, min(rear_side))
                if front_side:
                    hwF = max(hwF, abs(x))
            x += 0.2
        yy += 0.1
    if hwR > 0 or hwF > 0:
        prof.append((y, hwR, loR, hwF, loA))
    y += NOSE_STEP

CLR = 0.45
ZBOT = min(r[4] for r in prof) - CLR              # one flat floor for the whole relief
HSTEP = NOSE_STEP / 2.0                           # half-width of each cutter slice

# Sliced, not lofted - a lofted cutter silently no-ops when driven from inside
# this script (see the gotchas in the fit log), slices are reliable.
for k, (y, hwR, loR, hwF, loA) in enumerate(prof):
    if hwR > 0:
        box('CUT_nr%d' % k, -hwR - CLR, hwR + CLR, y - HSTEP, y + HSTEP, ZBOT, 1.6)
        boolean('VMU_RearShell', 'CUT_nr%d' % k)
    hw = max(hwR, hwF)
    if hw > 0:
        inner = min(hw, 9.3)
        box('CUT_nfa%d' % k, -inner - CLR, inner + CLR, y - HSTEP, y + HSTEP,
            ZBOT, XIAO_TOP + CLR)                 # stadium covers everything above
        boolean('VMU_FrontShell', 'CUT_nfa%d' % k)
        if hw > 9.3:
            box('CUT_nfb%d' % k, -hw - CLR, hw + CLR, y - HSTEP, y + HSTEP,
                ZBOT, OZ + CLR)                   # outboard of the XIAO: substrate only
            boolean('VMU_FrontShell', 'CUT_nfb%d' % k)

# ---------------------------------------------------------------- 5. JST pockets
pocket_floor = REAR_SKIN + REAR_WALL
for k, (lx, ly, w, h) in CONNS.items():
    wx, wy = Wx(lx), Wy(ly)
    box('CUT_p' + k, wx - w/2 - POCKET_CL, wx + w/2 + POCKET_CL,
        wy - h/2 - POCKET_CL, wy + h/2 + POCKET_CL, pocket_floor, 2.0)
    boolean('VMU_RearShell', 'CUT_p' + k)

# ---------------------------------------------------------------- 6. USB-C opening
UW, UH = 8.80, 3.16
USB_SHELL_LZ = -2.72
HW, HH = UW/2 + USB_CL, UH/2 + USB_CL
ZC = Wz(USB_SHELL_LZ)
R = HH; SX = HW - R
me = bpy.data.meshes.new('CUT_usb'); usb = bpy.data.objects.new('CUT_usb', me)
bpy.context.scene.collection.objects.link(usb)
bm = bmesh.new(); prof2 = []; N = 24
for i in range(N + 1):
    a = -math.pi/2 + math.pi*i/N; prof2.append(( SX + R*math.cos(a), ZC + R*math.sin(a)))
for i in range(N + 1):
    a =  math.pi/2 + math.pi*i/N; prof2.append((-SX + R*math.cos(a), ZC + R*math.sin(a)))
r0 = [bm.verts.new((x, -24.0, z)) for x, z in prof2]
r1 = [bm.verts.new((x, -42.0, z)) for x, z in prof2]
for i in range(len(prof2)):
    j = (i + 1) % len(prof2)
    bm.faces.new((r0[i], r0[j], r1[j], r1[i]))
bm.faces.new(r0[::-1]); bm.faces.new(r1)
bmesh.ops.recalc_face_normals(bm, faces=bm.faces[:])
bm.to_mesh(me); bm.free(); usb.display_type = 'WIRE'
boolean('VMU_RearShell', 'CUT_usb')
boolean('VMU_FrontShell', 'CUT_usb')

# ---------------------------------------------------------------- 7. battery notch (MAX)
# Hollow the front dome across its whole usable footprint.  Per x-slice, take the
# y-run of columns that are open cavity below the dome (first crossing >= 3.0 -
# this automatically excludes side walls, the x +-17.5 rib band, and anything
# hanging into the cavity) and cut up to (measured outer skin) - NOTCH_WALL.
# All slices are measured from the shell state BEFORE any notch cut, then cut in
# one pass - measuring as we go would see our own ceilings and self-limit.
HF_raw = caster('VMU_FrontShell')

def slice_spec(x0, x1):
    """(y0, y1, ceiling) for one x-slice, or None if nothing qualifies."""
    run_lo, run_hi, outer_min = -NOTCH_YCLAMP, NOTCH_YCLAMP, 99.0
    for sx in (x0 + 0.05, (x0 + x1) / 2, x1 - 0.05):
        # find the qualifying run containing y=0 at this x
        lo = None; hi = None
        y = 0.0
        while y >= -NOTCH_YCLAMP:
            c = HF_raw(sx, y)
            if len(c) >= 2 and c[0] >= 3.0:
                lo = y; outer_min = min(outer_min, c[-1])
            else:
                break
            y -= 0.5
        y = 0.5
        while y <= NOTCH_YCLAMP:
            c = HF_raw(sx, y)
            if len(c) >= 2 and c[0] >= 3.0:
                hi = y; outer_min = min(outer_min, c[-1])
            else:
                break
            y += 0.5
        if lo is None or hi is None:
            return None
        run_lo = max(run_lo, lo); run_hi = min(run_hi, hi)
    y0, y1 = run_lo + 0.3, run_hi - 0.3
    ceil = outer_min - NOTCH_WALL
    if y1 - y0 < 1.0 or ceil < NOTCH_FLOOR + 0.2:
        return None
    return (y0, y1, ceil)

specs = []
x = -NOTCH_XMAX
while x < NOTCH_XMAX - 1e-6:
    x1 = min(x + SLICE, NOTCH_XMAX)
    s = slice_spec(x, x1)
    if s:
        specs.append([x, x1, s[0], s[1], s[2]])
    x = x1

# Merge adjacent slices whose ceilings and runs are close - the crown is nearly
# flat, so this collapses ~70 slices to ~25 boolean cuts with <=0.12 mm loss.
# Bound the group's ceiling RANGE (hi-lo), not the step to the running min -
# comparing against the min alone chain-merges the whole descending flank into
# one ever-deepening cut (this happened: one 21 mm cut at the crown, 0.7 low).
merged = []; ghi = None
for s in specs:
    if merged:
        m = merged[-1]
        if (max(ghi, s[4]) - min(m[4], s[4]) <= 0.12 and abs(s[2] - m[2]) <= 0.6
                and abs(s[3] - m[3]) <= 0.6 and abs(s[0] - m[1]) < 1e-6):
            m[1] = s[1]
            m[2] = max(m[2], s[2]); m[3] = min(m[3], s[3])
            m[4] = min(m[4], s[4]); ghi = max(ghi, s[4])
            continue
    merged.append(list(s)); ghi = s[4]

print("notch: %d slices -> %d cuts" % (len(specs), len(merged)))
for k, (x0, x1, y0, y1, cz) in enumerate(merged):
    print("  cut %2d  x[%6.2f,%6.2f]  y[%6.1f,%6.1f]  ceil %.2f" % (k, x0, x1, y0, y1, cz))
    box('CUT_notch%d' % k, x0, x1, y0, y1, NOTCH_FLOOR, cz)
    boolean('VMU_FrontShell', 'CUT_notch%d' % k)

# ---------------------------------------------------------------- 8. button bore
SWX, SWY = Wx(SW1_L[0]), Wy(SW1_L[1])
SW_TOP = Wz(SW1_TOP_L)
HR = caster('VMU_RearShell')
hh = HR(SWX, SWY)
OUTER, FLOORZ = hh[0], hh[1]
bore = cyl('CUT_bore', SWX, SWY, 1.30, OUTER - 0.6, FLOORZ + 0.5, seg=48)
bore.display_type = 'WIRE'
boolean('VMU_RearShell', 'CUT_bore')

parts = [cyl('p_shaft',  SWX, SWY, 1.10, OUTER - 0.30, FLOORZ + 0.10, 48),
         cyl('p_flange', SWX, SWY, 2.50, FLOORZ + 0.10, FLOORZ + 1.10, 48),
         cyl('p_post',   SWX, SWY, 1.30, FLOORZ + 1.10, SW_TOP - 0.20, 48)]
bpy.ops.object.select_all(action='DESELECT')
for p in parts: p.select_set(True)
bpy.context.view_layer.objects.active = parts[0]
bpy.ops.object.join()
pl = bpy.context.active_object; pl.name = 'Button_Plunger'; pl.color = (0.95, 0.35, 0.10, 1)

# ---------------------------------------------------------------- 9. supports
def blo(x, y):
    h = HP(x, y)
    return min(h) if h else None

def disc_clear(cx, cy, r):
    for i in range(13):
        a = 2*math.pi*i/13
        for rr in (0.0, r*0.55, r, r + 0.3):
            v = blo(cx + rr*math.cos(a), cy + rr*math.sin(a))
            if v is None or v < PAD_TOP - 0.02:
                return False
    return True

def find_spot(cx, cy, r, span=7.0):
    best = None
    n = int(span*2)
    for dx in [i*0.5 for i in range(-n, n + 1)]:
        for dy in [i*0.5 for i in range(-n, n + 1)]:
            x, y = cx + dx, cy + dy
            if disc_clear(x, y, r):
                d = dx*dx + dy*dy
                if best is None or d < best[0]:
                    best = (d, x, y)
    return (best[1], best[2]) if best else None

sup = []
BOARD_UNDER, BOARD_TOP = -2.465, -1.355
PIN_CL    = 0.25                                  # diametral clearance, pin in hole
PIN_PROUD = 1.20                                  # how far the pin stands above the board
POST_HOLES = [(-12.76, -29.95, 1.57),             # primary pair, widest spacing
              ( 12.78, -29.95, 1.57),
              (  4.98,  -3.78, 2.10),             # largest hole -> sturdiest pin
              ( -1.55,  16.17, 1.57)]             # the only one near the +Y end
for i, (hx, hy, dia) in enumerate(POST_HOLES):
    r_pin = max((dia - PIN_CL) / 2.0, 0.55)
    def shoulder_ok(cx, cy, r):
        for k in range(13):
            a = 2*math.pi*k/13
            for rr in (r*0.6, r, r + 0.3):
                v = blo(cx + rr*math.cos(a), cy + rr*math.sin(a))
                if v is not None and v < PAD_TOP - 0.02:
                    return False
        return True
    r_sh  = 2.00
    while r_sh > r_pin + 0.35 and not shoulder_ok(hx, hy, r_sh):
        r_sh -= 0.25                              # shrink the shoulder off any component
    if not shoulder_ok(hx, hy, r_sh):
        print("  post %d at (%.2f,%.2f) skipped - no clear shoulder" % (i, hx, hy))
        continue
    sup.append(cyl('sp_sh%d' % i, hx, hy, r_sh,  FLOOR,     PAD_TOP))
    sup.append(cyl('sp_pn%d' % i, hx, hy, r_pin, PAD_TOP,   BOARD_TOP + PIN_PROUD))
    print("  post %d at (%6.2f,%6.2f)  shoulder O%.2f  pin O%.2f through a O%.2f hole"
          % (i, hx, hy, r_sh*2, r_pin*2, dia))
cn1x, cn1y = Wx(CONNS['CN1'][0]), Wy(CONNS['CN1'][1])
targets = [(-13, Wy(8.0)), (13, Wy(8.0)),
           (-13, Wy(-8.0)), (13, Wy(-8.0)),
           (cn1x, cn1y - 6.0),                    # in front of the Maple connector
           (-3.5, Wy(-26.0))]
placed = []
for (tx, ty) in targets:
    p = find_spot(tx, ty, 1.75)
    if p:
        sup.append(cyl('sp_%d' % len(sup), p[0], p[1], 1.75, FLOOR, PAD_TOP))
        placed.append((round(p[0], 2), round(p[1], 2)))
# Keep supports as separate closed solids - boolean unions dropped bodies
# intermittently here; every slicer unions overlapping bodies at slice time.
for i, s in enumerate(sup):
    s.name = 'SUP_%02d' % i
    s.color = (0.85, 0.55, 0.15, 1)
SUPPORT_NAMES = [s.name for s in sup]

# ---------------------------------------------------------------- 10. ghost cells + clearance
# Place each candidate cell as far toward +Y as the cavity taper allows (long
# cells have to ride over the XIAO), resting on the tallest thing under its
# footprint, and report the measured gap to the notch ceiling.  This is a
# REPORT, not a gate - nominal cell dims already proved pessimistic once.
HF_cut = caster('VMU_FrontShell')

def front_obstruction(x, y):
    h = HP(x, y)
    if not h: return None
    top = max(h)
    return top if top > OZ else OZ                # bare board face at OZ

def cell_report(name, T, W, L):
    y1 = 23.2; y0 = y1 - L                        # taper stops a 30 mm cell at +23.5
    # rest height: tallest board feature under the footprint
    rest = OZ
    yy = y0
    while yy <= y1:
        xx = -W/2 + 0.5
        while xx <= W/2 - 0.5:
            v = front_obstruction(xx, yy)
            if v is not None: rest = max(rest, v)
            xx += 1.0
        yy += 1.0
    bot = rest + CELL_GAP; top = bot + T
    # clearance to the cut shell over the footprint.  Report the full-thickness
    # BODY (inset 2.5 mm from the cell sides) separately from the outer EDGE
    # ring - a pouch cell's edges are thin seal flange, not full thickness, so
    # a negative edge number is not automatically a no-fit.
    worst_b = 99.0; at_b = None; worst_e = 99.0; at_e = None
    yy = y0
    while yy <= y1:
        # 0.73 step, not 0.8 - 0.8 lands exactly on the 0.5-grid cutter seams,
        # where a grazing ray reads the RAW ceiling and fakes a collision
        xx = -W/2 + 0.4
        while xx <= W/2 - 0.4 + 1e-6:
            c = HF_cut(xx, yy)
            if c:
                gap = c[0] - top
                if abs(xx) <= W/2 - 2.5:
                    if gap < worst_b: worst_b = gap; at_b = (round(xx,1), round(yy,1), round(c[0],2))
                else:
                    if gap < worst_e: worst_e = gap; at_e = (round(xx,1), round(yy,1), round(c[0],2))
            xx += 0.73
        yy += 1.0
    print("cell %s (%gx%gx%g): rides at z[%.2f,%.2f] y[%.1f,%.1f]" % (name, T, W, L, bot, top, y0, y1))
    print("  body gap %+.2f mm at %s   edge gap %+.2f mm at %s" % (worst_b, at_b, worst_e, at_e))
    me = bpy.data.meshes.new('Battery_' + name)
    bat = bpy.data.objects.new('Battery_' + name, me)
    bpy.context.scene.collection.objects.link(bat)
    bm = bmesh.new(); bmesh.ops.create_cube(bm, size=1.0)
    bmesh.ops.scale(bm, vec=Vector((W, L, T)), verts=bm.verts)
    bmesh.ops.bevel(bm, geom=list(bm.verts)+list(bm.edges)+list(bm.faces),
                    offset=0.6, segments=3, affect='EDGES', profile=0.5)
    bm.to_mesh(me); bm.free()
    bat.location = (0.0, (y0 + y1)/2, (bot + top)/2)
    bat.color = (0.15, 0.35, 0.85, 1)
    return worst_b

for (nm, T, W, L) in CELLS:
    cell_report(nm, T, W, L)

# ---------------------------------------------------------------- 11. finish + verify
nm_r = cleanup('VMU_RearShell')
nm_f = cleanup('VMU_FrontShell')
for o in bpy.data.objects:
    if o.name.startswith(('CUT_', 'p_')):
        o.hide_viewport = True; o.hide_render = True

# wall check over the notch: solid spans (filtering <0.15 mm boolean flakes)
HF_fin = caster('VMU_FrontShell')
min_wall = 99.0; wall_at = None
for (x0, x1, y0, y1, cz) in merged:
    for sx in (x0 + 0.1, (x0+x1)/2, x1 - 0.1):
        yy = y0 + 0.2
        while yy <= y1 - 0.2 + 1e-6:
            c = HF_fin(sx, yy)
            spans = [(c[i+1] - c[i], c[i]) for i in range(0, len(c) - 1, 2)]
            spans = [s for s in spans if s[0] >= 0.15 and s[1] > NOTCH_FLOOR]
            if spans:
                w = spans[-1][0]
                if w < min_wall: min_wall = w; wall_at = (round(sx,1), round(yy,1))
            yy += 2.0
print("notch min wall %.2f mm at %s (target %.2f)" % (min_wall, wall_at, NOTCH_WALL))

REAR_PARTS = ['VMU_RearShell'] + SUPPORT_NAMES

# ---------------------------------------------------------------- 12. export
os.makedirs(OUT, exist_ok=True)
def export(names, fname):
    bpy.ops.object.select_all(action='DESELECT')
    for n in names:
        bpy.data.objects[n].select_set(True)
    bpy.context.view_layer.objects.active = bpy.data.objects[names[0]]
    bpy.ops.wm.stl_export(filepath=OUT + fname, export_selected_objects=True)
    print("exported", fname)

export(['VMU_FrontShell'], 'pulsar_vmu_front_bigcell.stl')
export(REAR_PARTS, 'pulsar_vmu_rear.stl')
export(['Button_Plunger'], 'pulsar_vmu_button_plunger.stl')

bpy.ops.wm.save_as_mainfile(filepath=BF + 'pulsar_vmu_fit_assembly_bigcell.blend')

print("=== BUILD DONE (bigcell) ===")
print("rear half is %d bodies: shell + %d supports (identical to 703035 build)"
      % (len(REAR_PARTS), len(SUPPORT_NAMES)))
print("pads   %s" % (placed,))
print("non-manifold: rear %d, front %d" % (nm_r, nm_f))
