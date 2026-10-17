package com.example.graphrd;

import jakarta.ws.rs.Consumes;
import jakarta.ws.rs.POST;
import jakarta.ws.rs.Path;
import jakarta.ws.rs.PathParam;
import jakarta.ws.rs.Produces;
import jakarta.ws.rs.core.MediaType;
import jakarta.ws.rs.core.Response;

import org.eclipse.microprofile.rest.client.inject.RegisterRestClient;

/**
 * GraphRD's repository protocol as a MicroProfile REST client. The base URL
 * comes from {@code quarkus.rest-client.graphrd.url} in application.properties.
 */
@RegisterRestClient(configKey = "graphrd")
@Path("")
public interface GraphrdApi {

    /** Creates a repository; 409 when it already exists. */
    @POST
    @Path("/rest/repositories")
    @Consumes(MediaType.APPLICATION_JSON)
    Response createRepository(String config);

    /** Adds statements to the default graph from a JSON-LD document. */
    @POST
    @Path("/repositories/{repository}/statements")
    @Consumes("application/ld+json")
    Response loadJsonLd(@PathParam("repository") String repository, String jsonLd);

    /** Runs a SPARQL SELECT; the answer comes as SPARQL-Results JSON. */
    @POST
    @Path("/repositories/{repository}")
    @Consumes("application/sparql-query")
    @Produces("application/sparql-results+json")
    String select(@PathParam("repository") String repository, String sparql);
}
