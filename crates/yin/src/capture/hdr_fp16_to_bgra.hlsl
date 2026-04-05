cbuffer CursorParams : register(b0)
{
    int2 cursorOrigin;
    uint2 cursorSize;
    uint cursorRowBytes;
    uint cursorType;
    uint cursorVisible;
    uint sourceTransfer;
    uint4 _padding0;
};

Texture2D<float4> sourceTex : register(t0);
Texture2D<uint4> colorCursorTex : register(t1);
Texture2D<uint> monoCursorTex : register(t2);

struct VsOut
{
    float4 position : SV_Position;
};

static const uint SOURCE_TRANSFER_SRGB = 0;
static const uint SOURCE_TRANSFER_LINEAR_FP16 = 1;

float3 linear_to_srgb_fast(float3 linearRgb)
{
    linearRgb = saturate(linearRgb);

    float3 s1 = sqrt(linearRgb);
    float3 s2 = sqrt(s1);
    float3 s3 = sqrt(s2);

    float3 approxHigh = 0.662002687 * s1
        + 0.684122060 * s2
        - 0.323583601 * s3
        - 0.0225411470 * linearRgb;
    float3 exactLow = linearRgb * 12.92;

    return saturate(lerp(approxHigh, exactLow, step(linearRgb, 0.0031308.xxx)));
}

VsOut vs_main(uint vertexId : SV_VertexID)
{
    static const float2 positions[3] = {
        float2(-1.0, -1.0),
        float2(-1.0,  3.0),
        float2( 3.0, -1.0),
    };

    VsOut output;
    output.position = float4(positions[vertexId], 0.0, 1.0);
    return output;
}

float4 ps_main(VsOut input) : SV_Target
{
    uint2 pixel = uint2(input.position.xy);
    float4 source = sourceTex.Load(int3(pixel, 0));
    float3 srcRgb = source.rgb;
    if (sourceTransfer == SOURCE_TRANSFER_LINEAR_FP16)
    {
        srcRgb = linear_to_srgb_fast(srcRgb);
    }
    else
    {
        srcRgb = saturate(srcRgb);
    }

    uint4 src = (uint4)round(float4(srcRgb, 1.0) * 255.0);
    uint3 rgb = src.rgb;

    if (cursorVisible != 0 && cursorSize.x != 0 && cursorSize.y != 0)
    {
        int2 rel = int2(pixel) - cursorOrigin;
        if (rel.x >= 0 && rel.y >= 0 && rel.x < (int)cursorSize.x && rel.y < (int)cursorSize.y)
        {
            uint cx = (uint)rel.x;
            uint cy = (uint)rel.y;

            if (cursorType == 2)
            {
                uint4 cursor = colorCursorTex.Load(int3(cx, cy, 0));
                uint alpha = cursor.a;
                if (alpha != 0)
                {
                    uint invAlpha = 255 - alpha;
                    rgb = (cursor.rgb * alpha + rgb * invAlpha) / 255;
                }
            }
            else if (cursorType == 4)
            {
                uint4 cursor = colorCursorTex.Load(int3(cx, cy, 0));
                if (cursor.a == 255)
                {
                    rgb ^= cursor.rgb;
                }
                else
                {
                    rgb = cursor.rgb;
                }
            }
            else if (cursorType == 1)
            {
                uint byteColumn = cx >> 3;
                uint bit = 0x80u >> (cx & 7u);
                uint andByte = monoCursorTex.Load(int3(byteColumn, cy, 0));
                uint xorByte = monoCursorTex.Load(int3(byteColumn, cy + cursorSize.y, 0));
                bool andBit = (andByte & bit) != 0;
                bool xorBit = (xorByte & bit) != 0;

                if (!andBit && !xorBit)
                {
                    rgb = uint3(0, 0, 0);
                }
                else if (!andBit && xorBit)
                {
                    rgb = uint3(255, 255, 255);
                }
                else if (andBit && xorBit)
                {
                    rgb = uint3(255, 255, 255) - rgb;
                }
            }
        }
    }

    return float4(float3(rgb) / 255.0, 1.0);
}
