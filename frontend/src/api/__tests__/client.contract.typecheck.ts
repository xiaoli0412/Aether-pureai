import apiClient from '@/api/client'

type Assert<T extends true> = T
type IsAny<T> = 0 extends (1 & T) ? true : false
type IsEqual<Left, Right> = (
  <Value>() => Value extends Left ? 1 : 2
) extends <Value>() => Value extends Right ? 1 : 2
  ? true
  : false

function verifyUntypedApiClientResponsesMatchAxiosDefaults(): void {
  const requestResponse = apiClient.request({ url: '/__typecheck__/request' })
  const getResponse = apiClient.get('/__typecheck__/get')
  const postResponse = apiClient.post('/__typecheck__/post')
  const putResponse = apiClient.put('/__typecheck__/put')
  const patchResponse = apiClient.patch('/__typecheck__/patch')
  const deleteResponse = apiClient.delete('/__typecheck__/delete')

  type RequestData = Awaited<typeof requestResponse>['data']
  type GetData = Awaited<typeof getResponse>['data']
  type PostData = Awaited<typeof postResponse>['data']
  type PutData = Awaited<typeof putResponse>['data']
  type PatchData = Awaited<typeof patchResponse>['data']
  type DeleteData = Awaited<typeof deleteResponse>['data']

  type _RequestUsesAxiosDefault = Assert<IsAny<RequestData>>
  type _GetUsesAxiosDefault = Assert<IsAny<GetData>>
  type _PostUsesAxiosDefault = Assert<IsAny<PostData>>
  type _PutUsesAxiosDefault = Assert<IsAny<PutData>>
  type _PatchUsesAxiosDefault = Assert<IsAny<PatchData>>
  type _DeleteUsesAxiosDefault = Assert<IsAny<DeleteData>>
}

interface ExplicitResponse {
  id: string
}

function verifyExplicitResponseTypesRemainPrecise(): void {
  const response = apiClient.get<ExplicitResponse>('/__typecheck__/explicit')

  type ResponseData = Awaited<typeof response>['data']
  type _ExplicitTypeIsPreserved = Assert<IsEqual<ResponseData, ExplicitResponse>>
}

void verifyUntypedApiClientResponsesMatchAxiosDefaults
void verifyExplicitResponseTypesRemainPrecise
