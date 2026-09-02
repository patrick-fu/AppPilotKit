package dev.apppilotkit.transport;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

public final class NativeTransport {
    private static final int ABI_VERSION = 0x00010000;
    private static final int OUTCOME_SIZE = 112;
    private static final int OUTCOME_ENDPOINT_READY = 1;
    private static final int STATUS_OK = 0;
    private static final int STATUS_INVALID_HANDLE = -3;

    private NativeTransport() {}

    private static native int abiVersion();
    private static native long create(ByteBuffer descriptor, long descriptorLen, ByteBuffer outcome);
    private static native int drive(
            long handle,
            int tag,
            int flags,
            long streamId,
            long writeToken,
            ByteBuffer bytes,
            long bytesLen,
            ByteBuffer outcome);
    private static native int close(long handle, ByteBuffer outcome);
    private static native int drop(long handle);
    private static native long outputCount(long handle);
    private static native long outputLen(long output);
    private static native long outputCopy(long output, ByteBuffer destination, long capacity);
    private static native int outputDrop(long output);

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            throw new AssertionError("expected native library and D0 vector paths");
        }
        System.load(args[0]);
        check(abiVersion() == ABI_VERSION, "ABI version");

        String vector = Files.readString(Path.of(args[1]), StandardCharsets.UTF_8);
        byte[] descriptor = decodeHex(vectorString(vector, "launch_descriptor_cbor_hex"));
        byte[] expected = vectorString(vector, "localabstract_name").getBytes(StandardCharsets.UTF_8);
        check(expected.length >= 32 && expected.length <= 96, "D0 endpoint length");
        for (byte value : expected) {
            check(value != 0, "D0 endpoint contains NUL");
        }

        ByteBuffer descriptorBuffer = ByteBuffer.allocateDirect(descriptor.length);
        descriptorBuffer.put(descriptor).rewind();
        ByteBuffer outcome = ByteBuffer.allocateDirect(OUTCOME_SIZE).order(ByteOrder.nativeOrder());
        long handle = create(descriptorBuffer, descriptor.length, outcome);
        check(handle > 0, "create handle " + handle);
        check(outcome.getInt(8) == OUTCOME_ENDPOINT_READY, "create outcome kind");
        check(outcome.getLong(40) == 1, "Android platform");
        check(outcome.getLong(48) == 0, "Android endpoint value");
        long output = outcome.getLong(32);
        check(output > 0, "endpoint output");
        check(outputCount(handle) == 1, "endpoint output count");
        check(outputLen(output) == expected.length, "endpoint output length");

        ByteBuffer copied = ByteBuffer.allocateDirect(expected.length + 1);
        copied.put(expected.length, (byte) 0x5a);
        check(outputCopy(output, copied, expected.length + 1) == expected.length, "endpoint copy");
        byte[] actual = new byte[expected.length];
        copied.position(0).get(actual);
        check(Arrays.equals(actual, expected), "endpoint bytes differ from D0 literal");
        check(copied.get(expected.length) == (byte) 0x5a, "endpoint copy appended NUL");

        check(outputDrop(output) == STATUS_OK, "output drop");
        check(outputCount(handle) == 0, "output count after drop");
        check(outputLen(output) == STATUS_INVALID_HANDLE, "stale output handle");
        check(drop(handle) == STATUS_OK, "supervisor drop");
    }

    private static String vectorString(String vector, String key) {
        String marker = "\"" + key + "\": \"";
        int start = vector.indexOf(marker);
        check(start >= 0, "missing D0 vector key " + key);
        start += marker.length();
        int end = vector.indexOf('"', start);
        check(end >= 0, "unterminated D0 vector key " + key);
        return vector.substring(start, end);
    }

    private static byte[] decodeHex(String hex) {
        check((hex.length() & 1) == 0, "odd D0 descriptor hex");
        byte[] decoded = new byte[hex.length() / 2];
        for (int index = 0; index < decoded.length; index++) {
            int high = Character.digit(hex.charAt(index * 2), 16);
            int low = Character.digit(hex.charAt(index * 2 + 1), 16);
            check(high >= 0 && low >= 0, "invalid D0 descriptor hex");
            decoded[index] = (byte) ((high << 4) | low);
        }
        return decoded;
    }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
