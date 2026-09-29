# SPDX-License-Identifier: GPL-3.0-or-later
"""Render the VMU pulsar animation in Blender, headless.

    Blender -b -P render_pulsar.py -- <outdir> [frames]

A lighthouse pulsar: the spin axis is tipped toward the camera, and the magnetic
axis (which carries the two beams) sits off it, so each turn sweeps a beam through
the line of sight. That sweep, not the model, is what survives at 48x32 1-bit.

Writes <outdir>/fNN.png (8x supersampled greyscale) and <outdir>/angles.json, the
angle in degrees between the nearer beam and the line of sight, per frame —
bake_frames.py drives the flash from it, so the geometry lives only here.
"""
import json
import math
import sys

import bpy
from mathutils import Matrix, Vector

argv = sys.argv[sys.argv.index("--") + 1:]
OUT = argv[0]
FRAMES = int(argv[1]) if len(argv) > 1 else 12

SS = 8                         # supersample: bake_frames.py box-filters back to 48x32
SPIN_TILT = math.radians(33)   # spin axis tipped toward the camera...
SPIN_ROLL = math.radians(-12)  # ...and leaned in the screen plane
MAG_ANGLE = math.radians(52)   # magnetic axis off the spin axis: the sweep passes 5 deg from the LOS
BEAM_LEN, BEAM_HALF = 16.0, math.radians(8)
FADE_LEN = 7.5                 # beams fade to nothing over the ~7 units that are on screen

bpy.ops.wm.read_factory_settings(use_empty=True)
sc = bpy.context.scene
sc.render.resolution_x, sc.render.resolution_y = 48 * SS, 32 * SS
sc.render.engine = "CYCLES"
sc.cycles.samples = 48
sc.cycles.seed = 0
sc.cycles.use_denoising = False
# Standard, not AgX/Filmic: the bake thresholds against linear-ish values.
sc.view_settings.view_transform = "Standard"
sc.render.image_settings.file_format = "PNG"

world = bpy.data.worlds.new("World")
sc.world = world
world.use_nodes = True
bg = next(n for n in world.node_tree.nodes if n.type == "BACKGROUND")
bg.inputs["Color"].default_value = (0, 0, 0, 1)

cam_data = bpy.data.cameras.new("Cam")
cam_data.type = "ORTHO"
cam_data.ortho_scale = 11.0
cam = bpy.data.objects.new("Cam", cam_data)
sc.collection.objects.link(cam)
sc.camera = cam
cam.location = (0, -30, 0)
cam.rotation_euler = (math.radians(90), 0, 0)
TO_CAMERA = Vector((0, -1, 0))

sun_data = bpy.data.lights.new("Sun", "SUN")
sun_data.energy = 4.0
sun = bpy.data.objects.new("Sun", sun_data)
sc.collection.objects.link(sun)
sun.rotation_euler = (math.radians(60), math.radians(-35), math.radians(-30))

# The star. bake_frames.py draws it as a crisp disc; this render only occludes the far beam.
bpy.ops.mesh.primitive_uv_sphere_add(radius=1.0, segments=48, ring_count=24)
star = bpy.context.object
bpy.ops.object.shade_smooth()
star_mat = bpy.data.materials.new("Star")
star_mat.use_nodes = True
star.data.materials.append(star_mat)
p = next(n for n in star_mat.node_tree.nodes if n.type == "BSDF_PRINCIPLED")
p.inputs["Base Color"].default_value = (1, 1, 1, 1)
p.inputs["Emission Color"].default_value = (1, 1, 1, 1)
p.inputs["Emission Strength"].default_value = 0.25

# spin: orients the spin axis, turned about its local Z per frame. mag: fixed offset.
spin = bpy.data.objects.new("Spin", None)
sc.collection.objects.link(spin)
mag = bpy.data.objects.new("Mag", None)
sc.collection.objects.link(mag)
mag.parent = spin
mag.rotation_euler = (MAG_ANGLE, 0, 0)

# Beam: additive emission fading along the cone, transparent elsewhere.
beam_mat = bpy.data.materials.new("Beam")
beam_mat.use_nodes = True
nt = beam_mat.node_tree
nt.nodes.clear()
coord = nt.nodes.new("ShaderNodeTexCoord")
sep = nt.nodes.new("ShaderNodeSeparateXYZ")
fade = nt.nodes.new("ShaderNodeMapRange")
fade.inputs["From Min"].default_value = -BEAM_LEN / 2  # the apex, in cone-local Z
fade.inputs["From Max"].default_value = -BEAM_LEN / 2 + FADE_LEN
fade.inputs["To Min"].default_value = 0.6
fade.inputs["To Max"].default_value = 0.0
emit = nt.nodes.new("ShaderNodeEmission")
clear = nt.nodes.new("ShaderNodeBsdfTransparent")
add = nt.nodes.new("ShaderNodeAddShader")
out = nt.nodes.new("ShaderNodeOutputMaterial")
nt.links.new(coord.outputs["Object"], sep.inputs[0])
nt.links.new(sep.outputs["Z"], fade.inputs["Value"])
nt.links.new(fade.outputs["Result"], emit.inputs["Strength"])
nt.links.new(emit.outputs[0], add.inputs[0])
nt.links.new(clear.outputs[0], add.inputs[1])
nt.links.new(add.outputs[0], out.inputs["Surface"])

radius = BEAM_LEN * math.tan(BEAM_HALF)
for sign in (1, -1):
    bpy.ops.mesh.primitive_cone_add(vertices=48, radius1=0.0, radius2=radius,
                                    depth=BEAM_LEN, end_fill_type="NOTHING")
    cone = bpy.context.object
    cone.data.materials.append(beam_mat)
    cone.parent = mag
    # Apex at the star centre, opening along +/-Z of the magnetic axis.
    cone.location = (0, 0, sign * BEAM_LEN / 2)
    cone.rotation_euler = (0 if sign > 0 else math.pi, 0, 0)

base = Matrix.Rotation(SPIN_ROLL, 4, "Y") @ Matrix.Rotation(SPIN_TILT, 4, "X")
angles = []
for f in range(FRAMES):
    spin.matrix_world = base @ Matrix.Rotation(2 * math.pi * f / FRAMES, 4, "Z")
    bpy.context.view_layer.update()
    beam = (mag.matrix_world.to_3x3() @ Vector((0, 0, 1))).normalized()
    angles.append(math.degrees(math.acos(min(1.0, abs(beam.dot(TO_CAMERA))))))
    sc.render.filepath = f"{OUT}/f{f:02d}.png"
    bpy.ops.render.render(write_still=True)

with open(f"{OUT}/angles.json", "w") as fh:
    json.dump(angles, fh)
