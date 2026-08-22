/* Compiles the generated ABI header so its static assertions actually run.
 *
 * The assertions compare the C compiler's idea of every struct's size, alignment and field
 * offsets against the numbers taken from the Rust types at generation time. Generating the
 * header proves nothing on its own — a C compiler has to agree with it, and this is the
 * smallest translation unit that makes one look.
 *
 * Built as both C and C++, because the mirror is C++ and the header claims to serve both.
 */
#include "tessella_capture_abi.h"

int tsl_probe(void);

int tsl_probe(void) {
    return (int)(sizeof(tsl_camera_update) + sizeof(tsl_geometry_add) + sizeof(tsl_ring_control));
}
