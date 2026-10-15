// Print fan speeds (rpm) from the SMC: FNum, then F<i>Ac / F<i>Mx. No root needed.
#include <IOKit/IOKitLib.h>
#include <stdio.h>
#include <string.h>
#include <stdint.h>
typedef struct { uint8_t major, minor, build, reserved; uint16_t release; } Vers;
typedef struct { uint16_t version, length; uint32_t cpuPLimit, gpuPLimit, memPLimit; } PLimit;
typedef struct { uint32_t dataSize, dataType; uint8_t dataAttributes; } KeyInfo;
typedef struct { uint32_t key; Vers vers; PLimit plimit; KeyInfo keyInfo; uint8_t result, status, data8; uint32_t data32; uint8_t bytes[32]; } SMCParam;
static io_connect_t conn;
static uint32_t k4(const char *s) { return (uint32_t)s[0] << 24 | s[1] << 16 | s[2] << 8 | s[3]; }
static int call(SMCParam *in, SMCParam *out) { size_t n = sizeof(SMCParam); return IOConnectCallStructMethod(conn, 2, in, sizeof(SMCParam), out, &n); }
static int readkey(const char *key, SMCParam *out) {
  SMCParam in = {0}; in.key = k4(key); in.data8 = 9;
  if (call(&in, out) || out->result) return -1;
  in.keyInfo = out->keyInfo; in.data8 = 5; memset(out, 0, sizeof *out);
  if (call(&in, out) || out->result) return -1;
  out->keyInfo = in.keyInfo; return 0;
}
static double num(const char *key) {
  SMCParam o = {0}; if (readkey(key, &o)) return -1;
  if (o.keyInfo.dataType == k4("flt ")) { float f; memcpy(&f, o.bytes, 4); return f; }
  if (o.keyInfo.dataType == k4("fpe2")) return ((o.bytes[0] << 8) | o.bytes[1]) / 4.0;
  if (o.keyInfo.dataSize == 1) return o.bytes[0];
  return -1;
}
int main(void) {
  io_service_t s = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching("AppleSMC"));
  if (!s || IOServiceOpen(s, mach_task_self(), 0, &conn)) { fprintf(stderr, "no SMC\n"); return 1; }
  int n = (int)num("FNum");
  for (int i = 0; i < n; i++) {
    char a[5], m[5]; snprintf(a, 5, "F%dAc", i); snprintf(m, 5, "F%dMx", i);
    printf("fan%d %.0f/%.0f ", i, num(a), num(m));
  }
  printf("\n"); return 0;
}
