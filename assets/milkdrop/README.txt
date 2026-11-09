MilkDrop presets for FeAmp
==========================

These are visualisation presets for MilkDrop, the visualisation plug-in that
shipped with Winamp. FeAmp's visualiser (libs/milkdrop) runs them: open FeAmp,
start a song, and press the V button on the left of its main window, or
Ctrl+Shift+K.

Where they come from
--------------------

They are a selection from the original preset pack that was deployed with the
last official MilkDrop release, as kept by the projectM project:

    https://github.com/projectM-visualizer/presets-milkdrop-original
    commit e03b83e3338d8f1ed6cbcf908c719f249ef24288

The files are unmodified. Each preset's author is named at the start of its
file name (Aderrasi, Geiss, Rovastar, Unchained, Zylot, yin and others), and
the presets remain theirs: they are not covered by FeOS's own licence. They
are included here, as they were in Winamp and MilkDrop's own distribution, so
that the visualiser has something to show.

Which ones
----------

The pack has 552 presets. About half use MilkDrop 2 pixel shaders, which need
a GPU; FeAmp's renderer is software, so it uses everything in a preset except
the shaders. These 49 have no shaders, so they look as their authors meant.
They were chosen by rendering every shader-free preset to music and keeping
ones that look good and render quickly.

To add more, copy any .milk file into this directory (or anywhere ending in
.milk) and FeAmp's visualiser will find it.
