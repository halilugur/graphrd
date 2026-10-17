package com.example.personnel.model.entity;

import com.graphrd.rdf.RdfId;
import com.graphrd.rdf.RdfProperty;
import com.graphrd.rdf.RdfType;

import lombok.AllArgsConstructor;
import lombok.Getter;
import lombok.NoArgsConstructor;

/** A company in the graph; employees point at it by IRI. */
@Getter
@NoArgsConstructor
@AllArgsConstructor
@RdfType("Company")
public class Company {

    @RdfId
    String id;

    @RdfProperty("name")
    String name;

    @RdfProperty("industry")
    String industry;
}
