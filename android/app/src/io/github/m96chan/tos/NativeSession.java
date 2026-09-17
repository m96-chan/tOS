package io.github.m96chan.tos;

import android.view.Surface;

/** Private JNI boundary. All calls are made on MainActivity's session worker. */
final class NativeSession {
    static { System.loadLibrary("tos_android"); }
    static native long create(String home, float font);
    static native void surface(long handle, Surface surface);
    static native int step(long handle);
    static native void resize(long handle, int width, int height, float font);
    static native void text(long handle, String text, int modifiers, boolean paste);
    static native void key(long handle, int code, int unicode, int modifiers, boolean release);
    static native void pointer(long handle, float x, float y, int wheel);
    /** Phases of a long press dragging a selection: 0 press, 1 drag, 2 release. */
    static native void select(long handle, float x, float y, int phase);
    static native void action(long handle, int action);
    static native byte[] clipboard(long handle);
    static native long grid(long handle);
    static native void destroy(long handle);
}
