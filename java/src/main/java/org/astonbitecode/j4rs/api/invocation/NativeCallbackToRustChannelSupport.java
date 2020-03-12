package org.astonbitecode.j4rs.api.invocation;

import org.astonbitecode.j4rs.api.NativeInvocation;
import org.astonbitecode.j4rs.errors.InvocationException;
import org.astonbitecode.j4rs.rust.RustPointer;

/**
 * Performs native callbacks to Rust channels
 */
public class NativeCallbackToRustChannelSupport {
    private static native int docallbacktochannel(long channelPointerAddress, NativeInvocation inv);

    private RustPointer channelPointerOpt = null;

    static void initialize(String libname) {
        try {
            System.loadLibrary(libname);
        } catch(UnsatisfiedLinkError error) {
            System.err.println("The Callbacks are not initialized because the j4rs lib was not found. You may ignore this error if you don't use callbacks.");
            error.printStackTrace();
        }
    }

    /**
     * Perform a callback
     *
     * @param obj The {@link Object} to pass in the callback.
     */
    public void doCallback(Object obj) {
        if (channelPointerOpt != null && obj != null) {
            docallbacktochannel(channelPointerOpt.getAddress(), new JsonInvocationImpl(obj, obj.getClass()));
        } else {
            throw new InvocationException("Cannot do callback. Please make sure that you don't try to access this method while being in the constructor of your class (that extends NativeCallbackSupport)");
        }
    }

    final void initPointer(RustPointer p) {
        this.channelPointerOpt = p;
    }
}
