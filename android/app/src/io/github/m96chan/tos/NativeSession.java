package io.github.m96chan.tos;

import android.view.Surface;

/** Private JNI boundary. All calls are made on MainActivity's session worker. */
final class NativeSession {
    static { System.loadLibrary("tos_android"); }
    static final int BUTTON_NONE = -1, BUTTON_LEFT = 0, BUTTON_MIDDLE = 1, BUTTON_RIGHT = 2;
    static final int WHEEL_UP = 64, WHEEL_DOWN = 65, WHEEL_LEFT = 66, WHEEL_RIGHT = 67;
    static final int POINTER_PRESS = 0, POINTER_RELEASE = 1, POINTER_DRAG = 2, POINTER_MOTION = 3;
    static native long create(String home, float font);
    static native void surface(long handle, Surface surface);
    static native int step(long handle);
    static native void resize(long handle, int width, int height, float font);
    static native void text(long handle, String text, int modifiers, boolean paste);
    static native void key(long handle, int code, int unicode, int modifiers, boolean release);
    static native void focus(long handle, boolean gained);
    static native void pointer(long handle, float x, float y, int button, int action, int modifiers);
    /** Phases of a long press dragging a selection: 0 press, 1 drag, 2 release. */
    static native void select(long handle, float x, float y, int phase);
    static native void action(long handle, int action);
    static native byte[] clipboard(long handle);
    static native long grid(long handle);
    static native void destroy(long handle);
}
