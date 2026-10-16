"""Decode clear FLAC frames with the bundled reference decoder."""
import ctypes as C
import ctypes.util
import pathlib
import sys
from .state import Failure

P = C.c_void_p
Read = C.CFUNCTYPE(C.c_int, P, P, C.POINTER(C.c_size_t), P)
Write = C.CFUNCTYPE(C.c_int, P, P, C.POINTER(C.POINTER(C.c_int32)), P)
Error = C.CFUNCTYPE(None, P, C.c_int, P)


def library():
    root = getattr(sys, "_MEIPASS", None)
    if root:
        suffix = ".dll" if sys.platform == "win32" else ".dylib" if sys.platform == "darwin" else ".so"
        candidates = sorted(path for path in pathlib.Path(root).iterdir()
                            if path.is_file() and "flac" in path.name.lower()
                            and suffix in path.name.lower())
        name = str(candidates[0]) if candidates else None
    else:
        name = ctypes.util.find_library("FLAC")
    if not name:
        raise Failure("The packaged FLAC decoder is missing.")
    lib = C.CDLL(name)
    lib.FLAC__stream_decoder_new.restype = P
    lib.FLAC__stream_decoder_init_stream.argtypes = [P, Read, P, P, P, P, Write, P, Error, P]
    lib.FLAC__stream_decoder_init_stream.restype = C.c_int
    for name in ["process_until_end_of_stream", "finish", "get_channels", "get_bits_per_sample", "get_sample_rate", "get_blocksize"]:
        method = getattr(lib, "FLAC__stream_decoder_" + name)
        method.argtypes, method.restype = [P], C.c_uint
    lib.FLAC__stream_decoder_delete.argtypes = [P]
    return lib


def decode(data, bits, rate, expected):
    lib = library()
    decoder = lib.FLAC__stream_decoder_new()
    if not decoder:
        raise Failure("The FLAC decoder could not start.")
    output, errors = bytearray(), []
    position, samples, channels = 0, 0, 0

    @Read
    def read(_, buffer, size, __):
        nonlocal position
        count = min(size[0], len(data)-position)
        size[0] = count
        if count:
            C.memmove(buffer, data[position:position+count], count)
            position += count
            return 0
        return 1

    @Write
    def write(decoder, _, planes, __):
        nonlocal samples, channels
        count = lib.FLAC__stream_decoder_get_blocksize(decoder)
        current_channels = lib.FLAC__stream_decoder_get_channels(decoder)
        current_bits = lib.FLAC__stream_decoder_get_bits_per_sample(decoder)
        current_rate = lib.FLAC__stream_decoder_get_sample_rate(decoder)
        if current_bits != bits or current_rate != rate or not 1 <= current_channels <= 8 or (channels and channels != current_channels) or samples+count > expected:
            errors.append("Unexpected audio format")
            return 1
        channels = current_channels
        try:
            for sample in range(count):
                for channel in range(channels):
                    output.extend(planes[channel][sample].to_bytes(bits//8, "little", signed=True))
        except Exception:
            errors.append("Invalid sample")
            return 1
        samples += count
        return 0

    @Error
    def error(_, status, __):
        errors.append(status)

    try:
        status = lib.FLAC__stream_decoder_init_stream(decoder, read, None, None, None, None, write, None, error, None)
        if status or not lib.FLAC__stream_decoder_process_until_end_of_stream(decoder) or errors or samples != expected:
            raise Failure("The FLAC fragment does not match its sample timeline.")
        return bytes(output), channels
    finally:
        lib.FLAC__stream_decoder_finish(decoder)
        lib.FLAC__stream_decoder_delete(decoder)


def metadata(init):
    # The dfLa sample-entry box carries a full-box header then FLAC metadata.
    at = init.find(b"dfLa")
    if at < 4:
        raise Failure("The FLAC initialization metadata is missing.")
    size = int.from_bytes(init[at-4:at], "big")
    body = init[at+8:at-4+size]
    if len(body) < 38 or body[0] & 0x7f != 0 or int.from_bytes(body[1:4], "big") != 34:
        raise Failure("Invalid FLAC initialization metadata.")
    # STREAMINFO is sufficient to decode a fragment; do not carry unrelated tags.
    return b"fLaC\x80\x00\x00\x22" + body[4:38]


def payload(segment):
    position, result = 0, bytearray()
    while position + 8 <= len(segment):
        size = int.from_bytes(segment[position:position+4], "big")
        kind, header = segment[position+4:position+8], 8
        if size == 1:
            if position + 16 > len(segment):
                raise Failure("Invalid audio fragment.")
            size, header = int.from_bytes(segment[position+8:position+16], "big"), 16
        elif size == 0:
            size = len(segment)-position
        if size < header or position+size > len(segment):
            raise Failure("Invalid audio fragment.")
        if kind == b"mdat":
            result.extend(segment[position+header:position+size])
        position += size
    if position != len(segment) or not result:
        raise Failure("The audio fragment has no samples.")
    return bytes(result)
